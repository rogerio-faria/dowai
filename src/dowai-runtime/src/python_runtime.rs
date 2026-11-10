use dowai_core::{CancellationToken, DowaiError, ProjectPython, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

// IPC protocol (protocol_version 1, newline-delimited JSON, one message per line).
//
// Host -> worker, first line (request): `variables`, `dependencies`, and
// `mode` (additive field, defaults to "automatic" when absent).
//
// Worker -> host, zero or more `{"kind":"call"}` lines (additive, nested
// `env.run`), each carrying `target`, `mode` and the caller's current
// `variables` (the overlay, including pending setVar). The host answers each
// with exactly one line: `call_ok` (`variables` = merged overlay,
// `invocation_id`) or `call_failed` (`code`, `message`, `invocation_id`).
// The `message` of `call_failed` is a static text chosen by `code`; it never
// carries paths or other host detail (the full error stays on the host).
//
// Worker -> host, last line (final): `ok`/`failed`/`unsupported`/`protocol`
// exactly as before; `failed` may additionally carry the stable `code` of an
// uncaught DowaiExecutionError, plus the `invocation_id` of the failed call it
// carries (additive). The host propagates the code only when both match an
// error it produced for that very call. Nothing may follow the final line.
//
// Both host-side channels are bounded (`sync_channel`) so a flooding worker is
// throttled by backpressure instead of growing host memory; the host keeps
// draining the reader channel until the reader thread ends. Each line is also
// capped at MAX_IPC_BYTES and any line after the final one is a protocol error.
const IPC_PROTOCOL_VERSION: u32 = 1;
const MAX_IPC_BYTES: usize = 20 * 1024 * 1024;
const CHANNEL_CAPACITY: usize = 4;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const WORKER: &str = r#"
import contextlib
import json
import pathlib
import runpy
import sys
import types

_out = sys.stdout
_in = sys.stdin.buffer

def respond(value):
    _out.write(json.dumps(value, separators=(",", ":"), ensure_ascii=False) + "\n")
    _out.flush()

def _error_class():
    try:
        import dowai
        candidate = getattr(dowai, "DowaiExecutionError", None)
        if isinstance(candidate, type) and issubclass(candidate, Exception):
            return candidate
    except Exception:
        pass

    class DowaiExecutionError(RuntimeError):
        def __init__(self, code, message, *, hint=None, run_id=None, invocation_id=None):
            if not isinstance(code, str) or not code:
                raise ValueError("code must be a non-empty string")
            if not isinstance(message, str) or not message:
                raise ValueError("message must be a non-empty string")
            super().__init__(message)
            self.code = code
            self.hint = hint
            self.run_id = run_id
            self.invocation_id = invocation_id

    module = types.ModuleType("dowai")
    module.DowaiExecutionError = DowaiExecutionError
    sys.modules.setdefault("dowai", module)
    return DowaiExecutionError

def main():
    try:
        _out.reconfigure(encoding="utf-8")
    except Exception:
        pass
    request = json.loads(_in.readline())
    if request.get("protocol_version") != 1 or not isinstance(request.get("variables"), dict):
        respond({"protocol_version": 1, "kind": "protocol"})
        return
    if sys.prefix == sys.base_prefix:
        respond({"protocol_version": 1, "kind": "failed"})
        return
    mode = request.get("mode", "automatic")
    if mode not in ("automatic", "manual"):
        respond({"protocol_version": 1, "kind": "protocol"})
        return

    ExecutionError = _error_class()

    class Env:
        DowaiExecutionError = ExecutionError

        def __init__(self, variables, dependencies, mode):
            self._variables = variables
            self._dependencies = dependencies
            self._mode = mode

        @staticmethod
        def _name(name):
            if not isinstance(name, str) or not name.strip() or len(name.encode("utf-8")) > 256 or any(ord(char) < 32 or ord(char) == 127 for char in name):
                raise ValueError("invalid variable name")

        @property
        def mode(self):
            return self._mode

        @contextlib.contextmanager
        def manual(self):
            previous = self._mode
            self._mode = "manual"
            try:
                yield self
            finally:
                self._mode = previous

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

        def run(self, target, *, mode=None):
            if not isinstance(target, str) or not target.strip():
                raise ValueError("target must be a non-empty string")
            selected = self._mode if mode is None else mode
            if selected not in ("automatic", "manual"):
                raise ValueError("mode must be 'automatic' or 'manual'")
            json.dumps(self._variables)
            respond({"protocol_version": 1, "kind": "call", "target": target, "mode": selected, "variables": self._variables})
            line = _in.readline()
            if not line:
                raise RuntimeError("the DOWAI host closed the nested call channel")
            reply = json.loads(line)
            if reply.get("protocol_version") != 1:
                raise RuntimeError("invalid nested call reply")
            if reply.get("kind") == "call_ok" and isinstance(reply.get("variables"), dict):
                self._variables.clear()
                self._variables.update(reply["variables"])
                return {"invocation_id": reply.get("invocation_id"), "status": "SUCCEEDED"}
            if reply.get("kind") == "call_failed":
                raise ExecutionError(
                    str(reply.get("code") or "DOWAI_EXECUTION_FAILED"),
                    str(reply.get("message") or "Nested model invocation failed"),
                    invocation_id=reply.get("invocation_id"),
                )
            raise RuntimeError("invalid nested call reply")

        def ref(self, *args, **kwargs):
            raise NotImplementedError()

        source = ref
        write = ref
        generateCsv = ref

    dependencies = set(request.get("dependencies", []))
    for dependency in list(dependencies):
        dependencies.add(pathlib.PurePath(dependency).stem)
    environment = Env(request["variables"], dependencies, mode)
    try:
        with contextlib.redirect_stdout(sys.stderr):
            namespace = runpy.run_path(sys.argv[1], run_name="__main__")
            model = namespace.get("model")
            if not callable(model):
                raise RuntimeError()
            result = model(environment)
    except NotImplementedError:
        respond({"protocol_version": 1, "kind": "unsupported"})
        return
    except BaseException as error:
        code = getattr(error, "code", None) if isinstance(error, ExecutionError) else None
        failed_call = getattr(error, "invocation_id", None) if isinstance(error, ExecutionError) else None
        message = {"protocol_version": 1, "kind": "failed"}
        if isinstance(code, str):
            message["code"] = code
        if isinstance(failed_call, str):
            message["invocation_id"] = failed_call
        respond(message)
        return
    if result is not None:
        respond({"protocol_version": 1, "kind": "unsupported"})
    else:
        respond({"protocol_version": 1, "kind": "ok", "variables": environment._variables})

try:
    main()
except BaseException:
    respond({"protocol_version": 1, "kind": "failed"})
"#;

#[derive(Serialize)]
struct WorkerRequest<'a> {
    protocol_version: u32,
    variables: &'a BTreeMap<String, Value>,
    dependencies: &'a [String],
    mode: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerMessage {
    protocol_version: u32,
    kind: String,
    #[serde(default)]
    variables: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    invocation_id: Option<String>,
}

#[derive(Serialize)]
struct CallReply<'a> {
    protocol_version: u32,
    kind: &'a str,
    invocation_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    variables: Option<&'a BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// A nested `env.run()` request issued by a running model.
pub(crate) struct ChildCall {
    pub target: String,
    pub mode: Option<String>,
    pub variables: BTreeMap<String, Value>,
}

/// Result of a nested call, as decided by the host.
pub(crate) struct ChildOutcome {
    pub invocation_id: String,
    pub result: Result<BTreeMap<String, Value>>,
}

/// Parameters of one supervised model process.
pub(crate) struct ModelInvocation<'a> {
    pub project_root: &'a Path,
    pub interpreter: &'a Path,
    pub deadline: Instant,
    pub model_path: &'a Path,
    pub dependencies: &'a [String],
    pub mode: &'a str,
    pub cancel: &'a CancellationToken,
}

type CallHandler<'a> = dyn FnMut(ChildCall) -> ChildOutcome + 'a;

pub(crate) fn execute_model(
    invocation: &ModelInvocation<'_>,
    variables: &BTreeMap<String, Value>,
    on_call: &mut CallHandler<'_>,
) -> Result<BTreeMap<String, Value>> {
    let mut request = serde_json::to_vec(&WorkerRequest {
        protocol_version: IPC_PROTOCOL_VERSION,
        variables,
        dependencies: invocation.dependencies,
        mode: invocation.mode,
    })
    .map_err(|_| protocol_error())?;
    if request.len() > MAX_IPC_BYTES {
        return Err(protocol_error());
    }
    request.push(b'\n');

    invocation.cancel.check()?;
    let mut command = Command::new(invocation.interpreter);
    command
        .arg("-c")
        .arg(WORKER)
        .arg(invocation.model_path)
        .current_dir(invocation.project_root)
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

    let (write_sender, write_receiver) = mpsc::sync_channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let writer = thread::spawn(move || -> io::Result<()> {
        for message in write_receiver {
            input.write_all(&message)?;
            input.flush()?;
        }
        Ok(())
    });
    let (read_sender, read_receiver) = mpsc::sync_channel::<io::Result<Vec<u8>>>(CHANNEL_CAPACITY);
    let reader = thread::spawn(move || {
        let mut output = BufReader::new(output);
        loop {
            let mut line = Vec::new();
            match output
                .by_ref()
                .take((MAX_IPC_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)
            {
                Ok(0) => break,
                Ok(_) => {
                    let oversized = line.len() > MAX_IPC_BYTES;
                    if read_sender.send(Ok(line)).is_err() || oversized {
                        break;
                    }
                }
                Err(error) => {
                    let _ = read_sender.send(Err(error));
                    break;
                }
            }
        }
    });
    let _ = write_sender.send(request);

    let mut supervision = Supervision {
        deadline: invocation.deadline,
        cancel: invocation.cancel.clone(),
        messages: read_receiver,
        replies: write_sender,
        final_message: None,
        failed_calls: HashMap::new(),
    };
    let driven = supervision.drive(&mut child, on_call);
    let Supervision {
        replies,
        final_message,
        failed_calls,
        messages,
        ..
    } = supervision;

    let status = match driven {
        Ok(status) => status,
        Err(error) => {
            terminate_process_tree(&process_tree, &mut child)?;
            child
                .wait()
                .map_err(|_| python_failure("Could not reap the Python worker"))?;
            drop(replies);
            // Dropping the receiver unblocks a reader stuck on a full channel.
            drop(messages);
            let _ = writer.join();
            let _ = reader.join();
            return Err(error);
        }
    };
    terminate_descendants(&process_tree)?;
    drop(replies);
    writer
        .join()
        .map_err(|_| protocol_error())?
        .map_err(|_| protocol_error())?;
    let mut final_message = final_message;
    // Keep draining until the reader ends: it may be blocked on the bounded channel.
    for pending in messages.iter() {
        // Anything after the worker exited is either the final message or noise.
        let line = pending.map_err(|_| protocol_error())?;
        if line.len() > MAX_IPC_BYTES || final_message.is_some() {
            return Err(protocol_error());
        }
        let message = parse_message(&line)?;
        if message.kind == "call" {
            return Err(protocol_error());
        }
        final_message = Some(message);
    }
    reader.join().map_err(|_| protocol_error())?;
    if !status.success() {
        return Err(protocol_error());
    }
    let response = final_message.ok_or_else(protocol_error)?;
    if Instant::now() >= invocation.deadline {
        return Err(timeout_error());
    }
    match response.kind.as_str() {
        "ok" => response.variables.ok_or_else(protocol_error),
        "unsupported" => Err(DowaiError::NotImplemented(
            "This Python model uses an operation or return type not supported by the current runtime"
                .to_owned(),
        )),
        "failed" => {
            // Only trust a code that matches the error the host produced for
            // the very call the model re-raised.
            let mut failed_calls = failed_calls;
            let trusted = match (response.code.as_deref(), response.invocation_id.as_deref()) {
                (Some(code), Some(id)) => failed_calls
                    .remove(id)
                    .filter(|error| error.code() == code),
                _ => None,
            };
            Err(trusted.unwrap_or_else(|| {
                python_failure("Python model failed; model exception details are not exposed")
            }))
        }
        _ => Err(protocol_error()),
    }
}

struct Supervision {
    deadline: Instant,
    cancel: CancellationToken,
    messages: Receiver<io::Result<Vec<u8>>>,
    replies: SyncSender<Vec<u8>>,
    final_message: Option<WorkerMessage>,
    failed_calls: HashMap<String, DowaiError>,
}

impl Supervision {
    fn drive(
        &mut self,
        child: &mut Child,
        on_call: &mut CallHandler<'_>,
    ) -> Result<std::process::ExitStatus> {
        loop {
            // Checked first so a cancelled run never serves further nested calls; the
            // caller terminates the whole process tree on any error.
            self.cancel.check()?;
            while let Ok(line) = self.messages.try_recv() {
                self.handle(line, on_call)?;
            }
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(_) => return Err(python_failure("Could not monitor the Python worker")),
            }
            if Instant::now() >= self.deadline {
                return Err(timeout_error());
            }
            match self.messages.recv_timeout(POLL_INTERVAL) {
                Ok(line) => self.handle(line, on_call)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => thread::sleep(POLL_INTERVAL),
            }
        }
    }

    fn handle(&mut self, line: io::Result<Vec<u8>>, on_call: &mut CallHandler<'_>) -> Result<()> {
        let line = line.map_err(|_| protocol_error())?;
        if line.len() > MAX_IPC_BYTES || self.final_message.is_some() {
            return Err(protocol_error());
        }
        let message = parse_message(&line)?;
        if message.kind != "call" {
            self.final_message = Some(message);
            return Ok(());
        }
        let (Some(target), Some(variables)) = (message.target, message.variables) else {
            return Err(protocol_error());
        };
        let outcome = on_call(ChildCall {
            target,
            mode: message.mode,
            variables,
        });
        let reply = match &outcome.result {
            Ok(variables) => CallReply {
                protocol_version: IPC_PROTOCOL_VERSION,
                kind: "call_ok",
                invocation_id: &outcome.invocation_id,
                variables: Some(variables),
                code: None,
                message: None,
            },
            Err(error) => CallReply {
                protocol_version: IPC_PROTOCOL_VERSION,
                kind: "call_failed",
                invocation_id: &outcome.invocation_id,
                variables: None,
                code: Some(error.code()),
                message: Some(worker_visible_message(error.code())),
            },
        };
        let mut bytes = serde_json::to_vec(&reply).map_err(|_| protocol_error())?;
        if bytes.len() > MAX_IPC_BYTES {
            return Err(protocol_error());
        }
        bytes.push(b'\n');
        self.replies.send(bytes).map_err(|_| protocol_error())?;
        if let Err(error) = outcome.result {
            self.failed_calls.insert(outcome.invocation_id, error);
        }
        Ok(())
    }
}

fn parse_message(line: &[u8]) -> Result<WorkerMessage> {
    let message: WorkerMessage = serde_json::from_slice(line).map_err(|_| protocol_error())?;
    if message.protocol_version != IPC_PROTOCOL_VERSION {
        return Err(protocol_error());
    }
    Ok(message)
}

/// Static, path-free text for errors of nested calls as seen by the model.
fn worker_visible_message(code: &str) -> String {
    match code {
        "DOWAI_POLICY_PATH" | "DOWAI_PLAN_INVALID" => {
            "Nested model could not be resolved within the project"
        }
        "DOWAI_PLAN_CYCLE" => "Nested call rejected: call cycle detected",
        "DOWAI_EXECUTION_FAILED" => "Nested call rejected: execution limit reached",
        "DOWAI_CONFIG_INVALID" => "Nested call rejected: invalid call configuration",
        "DOWAI_NOT_IMPLEMENTED" => "Nested call target is not supported yet",
        "DOWAI_SESSION_INVALID" => "Nested call rejected: invalid session variables",
        "DOWAI_PYTHON_FAILED" => "Nested Python model failed",
        "DOWAI_PYTHON_TIMEOUT" => "Nested Python model exceeded its timeout",
        "DOWAI_PYTHON_PROTOCOL" => "Nested Python model returned an invalid IPC response",
        "DOWAI_POLICY_DENIED" => "Nested call denied by policy",
        "DOWAI_CANCELLED" => "Nested call cancelled",
        _ => "Nested model invocation failed",
    }
    .to_owned()
}

fn timeout_error() -> DowaiError {
    DowaiError::PythonTimeout("Python model exceeded its configured timeout".to_owned())
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
