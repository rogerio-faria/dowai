use dowai_core::{DowaiError, ProjectPython, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const IPC_PROTOCOL_VERSION: u32 = 1;
const MAX_IPC_BYTES: usize = 20 * 1024 * 1024;
const WORKER: &str = r#"
import contextlib
import json
import pathlib
import runpy
import sys

def respond(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":"), ensure_ascii=False))

try:
    request = json.loads(sys.stdin.buffer.read())
    if request.get("protocol_version") != 1 or not isinstance(request.get("variables"), dict):
        respond({"protocol_version": 1, "kind": "protocol"})
        raise SystemExit(0)
    if sys.prefix == sys.base_prefix:
        respond({"protocol_version": 1, "kind": "failed"})
        raise SystemExit(0)

    class Env:
        def __init__(self, variables, dependencies):
            self._variables = variables
            self._dependencies = dependencies

        @staticmethod
        def _name(name):
            if not isinstance(name, str) or not name.strip() or len(name.encode("utf-8")) > 256 or any(ord(char) < 32 or ord(char) == 127 for char in name):
                raise ValueError("invalid variable name")

        def setVar(self, name, value):
            self._name(name)
            self._variables[name] = value
            json.dumps(self._variables)

        def getVar(self, name, default=None):
            self._name(name)
            return self._variables.get(name, default)

        def unsetVar(self, name):
            self._name(name)
            existed = name in self._variables
            self._variables.pop(name, None)
            return existed

        def depends_on(self, name):
            if not isinstance(name, str) or name not in self._dependencies:
                raise ValueError("dependency was not declared in the execution plan")

        def ref(self, *args, **kwargs):
            raise NotImplementedError()

        source = ref
        run = ref
        write = ref
        generateCsv = ref

    dependencies = set(request.get("dependencies", []))
    for dependency in list(dependencies):
        dependencies.add(pathlib.PurePath(dependency).stem)
    environment = Env(request["variables"], dependencies)
    with contextlib.redirect_stdout(sys.stderr):
        namespace = runpy.run_path(sys.argv[1], run_name="__main__")
        model = namespace.get("model")
        if not callable(model):
            raise RuntimeError()
        result = model(environment)
    if result is not None:
        respond({"protocol_version": 1, "kind": "unsupported"})
    else:
        respond({"protocol_version": 1, "kind": "ok", "variables": environment._variables})
except NotImplementedError:
    respond({"protocol_version": 1, "kind": "unsupported"})
except BaseException:
    respond({"protocol_version": 1, "kind": "failed"})
"#;

#[derive(Serialize)]
struct WorkerRequest<'a> {
    protocol_version: u32,
    variables: &'a BTreeMap<String, Value>,
    dependencies: &'a [String],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerResponse {
    protocol_version: u32,
    kind: String,
    #[serde(default)]
    variables: Option<BTreeMap<String, Value>>,
}

pub(crate) fn execute_model(
    project_root: &Path,
    interpreter: &Path,
    timeout: Duration,
    model_path: &Path,
    dependencies: &[String],
    variables: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>> {
    let request = serde_json::to_vec(&WorkerRequest {
        protocol_version: IPC_PROTOCOL_VERSION,
        variables,
        dependencies,
    })
    .map_err(|_| protocol_error())?;
    if request.len() > MAX_IPC_BYTES {
        return Err(protocol_error());
    }

    let mut command = Command::new(interpreter);
    command
        .arg("-c")
        .arg(WORKER)
        .arg(model_path)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    apply_worker_environment(&mut command);
    configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|_| python_failure("Could not start the configured project interpreter"))?;
    let process_tree = match attach_process_tree(&mut child) {
        Ok(tree) => tree,
        Err(error) => {
            stop_worker(&mut child)?;
            return Err(error);
        }
    };
    let Some(mut input) = child.stdin.take() else {
        terminate_process_tree(&process_tree, &mut child)?;
        child
            .wait()
            .map_err(|_| python_failure("Could not reap the Python worker"))?;
        return Err(protocol_error());
    };
    let Some(output) = child.stdout.take() else {
        terminate_process_tree(&process_tree, &mut child)?;
        child
            .wait()
            .map_err(|_| python_failure("Could not reap the Python worker"))?;
        return Err(protocol_error());
    };
    let writer = thread::spawn(move || input.write_all(&request));
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        output
            .take((MAX_IPC_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {
                terminate_process_tree(&process_tree, &mut child)?;
                child
                    .wait()
                    .map_err(|_| python_failure("Could not reap the Python worker"))?;
                writer
                    .join()
                    .map_err(|_| protocol_error())?
                    .map_err(|_| protocol_error())?;
                reader
                    .join()
                    .map_err(|_| protocol_error())?
                    .map_err(|_| protocol_error())?;
                return Err(python_failure("Could not monitor the Python worker"));
            }
        }
        if Instant::now() >= deadline {
            terminate_process_tree(&process_tree, &mut child)?;
            child
                .wait()
                .map_err(|_| python_failure("Could not reap the timed-out Python worker"))?;
            writer
                .join()
                .map_err(|_| protocol_error())?
                .map_err(|_| protocol_error())?;
            reader
                .join()
                .map_err(|_| protocol_error())?
                .map_err(|_| protocol_error())?;
            return Err(DowaiError::PythonTimeout(
                "Python model exceeded its configured timeout".to_owned(),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    terminate_descendants(&process_tree)?;
    writer
        .join()
        .map_err(|_| protocol_error())?
        .map_err(|_| protocol_error())?;
    let output = reader
        .join()
        .map_err(|_| protocol_error())?
        .map_err(|_| protocol_error())?;
    if output.len() > MAX_IPC_BYTES || !status.success() {
        return Err(protocol_error());
    }
    let response: WorkerResponse = serde_json::from_slice(&output).map_err(|_| protocol_error())?;
    if response.protocol_version != IPC_PROTOCOL_VERSION {
        return Err(protocol_error());
    }
    match response.kind.as_str() {
        "ok" => response.variables.ok_or_else(protocol_error),
        "unsupported" => Err(DowaiError::NotImplemented(
            "This Python model uses an operation or return type not supported by the current runtime"
                .to_owned(),
        )),
        "failed" => Err(python_failure(
            "Python model failed; model exception details are not exposed",
        )),
        "protocol" => Err(protocol_error()),
        _ => Err(protocol_error()),
    }
}

pub(crate) fn validate_python_runtime(configuration: &ProjectPython) -> Result<()> {
    if !configuration.dependencies.is_empty()
        || configuration.allow_unlisted_packages
        || configuration.package_policy != "declared_and_authorized"
    {
        return Err(DowaiError::NotImplemented(
            "Python dependency installation and package authorization are not available yet"
                .to_owned(),
        ));
    }
    Ok(())
}

fn python_failure(message: &str) -> DowaiError {
    DowaiError::PythonFailure(message.to_owned())
}

fn protocol_error() -> DowaiError {
    DowaiError::PythonProtocol("Python worker returned an invalid IPC response".to_owned())
}

fn stop_worker(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .map_err(|_| python_failure("Could not monitor the Python worker"))?
        .is_none()
    {
        child
            .kill()
            .map_err(|_| python_failure("Could not stop the unmonitored Python worker"))?;
    }
    child
        .wait()
        .map_err(|_| python_failure("Could not reap the unmonitored Python worker"))?;
    Ok(())
}

fn apply_worker_environment(command: &mut Command) {
    command.env_clear();
    for name in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("PYTHONDONTWRITEBYTECODE", "1");
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
struct ProcessTree {
    process_group: i32,
}

#[cfg(unix)]
fn attach_process_tree(child: &mut Child) -> Result<ProcessTree> {
    let process_group = i32::try_from(child.id())
        .map_err(|_| python_failure("Invalid Python worker process identifier"))?;
    Ok(ProcessTree { process_group })
}

#[cfg(unix)]
fn terminate_process_tree(tree: &ProcessTree, child: &mut Child) -> Result<()> {
    terminate_group(tree)?;
    terminate_child(child)
}

#[cfg(unix)]
fn terminate_descendants(tree: &ProcessTree) -> Result<()> {
    terminate_group(tree)
}

#[cfg(windows)]
struct ProcessTree(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn attach_process_tree(child: &mut Child) -> Result<ProcessTree> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(python_failure(
            "Could not create a process group for the Python worker",
        ));
    }
    let tree = ProcessTree(handle);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            tree.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    let assigned = configured != 0
        && unsafe { AssignProcessToJobObject(tree.0, child.as_raw_handle().cast()) } != 0;
    if !assigned {
        return Err(python_failure(
            "Could not supervise the Python worker process tree",
        ));
    }
    Ok(tree)
}

#[cfg(windows)]
fn terminate_process_tree(tree: &ProcessTree, _child: &mut Child) -> Result<()> {
    let terminated =
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(tree.0, 1) };
    if terminated == 0 {
        return Err(python_failure(
            "Could not terminate the timed-out Python process tree",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn terminate_descendants(tree: &ProcessTree) -> Result<()> {
    let terminated =
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(tree.0, 0) };
    if terminated == 0 {
        return Err(python_failure(
            "Could not stop remaining Python worker processes",
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
struct ProcessTree;

#[cfg(not(any(unix, windows)))]
fn attach_process_tree(_child: &mut Child) -> Result<ProcessTree> {
    Ok(ProcessTree)
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_tree(_tree: &ProcessTree, child: &mut Child) -> Result<()> {
    child
        .kill()
        .map_err(|_| python_failure("Could not terminate the timed-out Python worker"))?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn terminate_descendants(_tree: &ProcessTree) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn terminate_group(tree: &ProcessTree) -> Result<()> {
    // The negative PID targets only the process group created for this worker.
    let result = unsafe { libc::kill(-tree.process_group, libc::SIGKILL) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(python_failure(
                "Could not terminate the Python worker process group",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn terminate_child(child: &mut Child) -> Result<()> {
    if child.kill().is_err()
        && child
            .try_wait()
            .map_err(|_| python_failure("Could not monitor the Python worker"))?
            .is_none()
    {
        return Err(python_failure(
            "Could not terminate the Python worker process",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_python_package_features_fail_explicitly() {
        let configuration = ProjectPython {
            interpreter: Path::new("python").to_path_buf(),
            timeout_seconds: 10,
            dependencies: vec!["pandas".to_owned()],
            package_policy: "declared_and_authorized".to_owned(),
            allow_unlisted_packages: false,
        };

        let error = validate_python_runtime(&configuration)
            .expect_err("package installation is not implemented");

        assert_eq!(error.code(), "DOWAI_NOT_IMPLEMENTED");
    }

    #[test]
    fn unsupported_python_package_policies_fail_explicitly() {
        let configuration = ProjectPython {
            interpreter: Path::new("python").to_path_buf(),
            timeout_seconds: 10,
            dependencies: Vec::new(),
            package_policy: "admin_allowlist".to_owned(),
            allow_unlisted_packages: false,
        };

        let error = validate_python_runtime(&configuration)
            .expect_err("package allowlists are not enforced by this runtime");

        assert_eq!(error.code(), "DOWAI_NOT_IMPLEMENTED");
    }
}
