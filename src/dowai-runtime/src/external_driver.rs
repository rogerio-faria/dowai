//! External write drivers (ENT-128): a Python driver, chosen by configuration, writes to a
//! destination the runtime has no built-in adapter for. No Rust recompilation is needed.
//!
//! Configuration (`workspace.json`, profile `type: "external"`):
//! - `driver`: relative `.py` path under the project's `drivers/` directory (no `..`).
//! - `params`: non-secret object passed to the driver. Literal secrets are refused.
//! - `env`: names of environment variables the driver receives (values are never stored).
//!
//! Driver interface (module-level functions, all required):
//! - `open(params, table, columns, mode, keys, confirm_replace)`: prepares one write session.
//! - `write_batch(rows) -> int`: writes one batch of mappings with exactly the declared columns.
//! - `commit() -> dict`: makes the session durable and returns the counts as a JSON-compatible dict.
//! - `abort()`: discards the session. The driver must also roll back when its process exits
//!   without `commit`, because the runtime kills the process after any failure.
//!
//! The driver runs in a supervised process started with the project interpreter in isolated
//! mode. Its stdout is reserved for the protocol; anything it prints goes to stderr. Failures
//! carry the driver's `DOWAI_*` code when it provides one, and a sanitized message otherwise.

use crate::executor::WorkerSettings;
use crate::external_write::{validate_write_request, WriteRequest};
use dowai_core::{DowaiError, Result, WorkspaceProfile};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;

pub(crate) const DRIVER_INVALID: &str = "DOWAI_WRITE_DRIVER_INVALID";
pub(crate) const DRIVER_FAILED: &str = "DOWAI_WRITE_DRIVER_FAILED";
pub(crate) const DRIVER_TIMEOUT: &str = "DOWAI_WRITE_DRIVER_TIMEOUT";
const PROTOCOL_VERSION: u32 = 1;
const MAX_MESSAGE_CHARS: usize = 256;

/// Host side of the driver shim. Module-level functions only; the driver's own stdout is
/// redirected so it cannot corrupt the protocol.
const SHIM: &str = r#"
import contextlib
import importlib.util
import json
import sys

_protocol = sys.stdout
_CODES = ("open", "write_batch", "commit", "abort")

def _send(value):
    _protocol.write(json.dumps(value, separators=(",", ":")) + "\n")
    _protocol.flush()

def _load(path):
    spec = importlib.util.spec_from_file_location("dowai_external_driver", path)
    if spec is None or spec.loader is None:
        raise ImportError("driver cannot be loaded")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    for name in _CODES:
        if not callable(getattr(module, name, None)):
            raise ImportError("driver interface is incomplete")
    return module

def _failure(error):
    code = getattr(error, "code", None)
    if not (isinstance(code, str) and code.startswith("DOWAI_") and len(code) <= 64):
        code = "DOWAI_WRITE_DRIVER_FAILED"
    message = str(error)[:256]
    return {"protocol_version": 1, "kind": "failed", "code": code, "message": message}

def main():
    try:
        driver = _load(sys.argv[1])
    except BaseException:
        _send({"protocol_version": 1, "kind": "failed", "code": "DOWAI_WRITE_DRIVER_INVALID", "message": "driver cannot be loaded or lacks the interface"})
        return
    _send({"protocol_version": 1, "kind": "ready"})
    for line in sys.stdin:
        try:
            message = json.loads(line)
            operation = message.get("op")
            with contextlib.redirect_stdout(sys.stderr):
                if operation == "open":
                    driver.open(message["profile"], message["table"], message["columns"], message["mode"], message.get("keys"), message["confirm_replace"])
                    _send({"protocol_version": 1, "kind": "ok"})
                elif operation == "rows":
                    count = driver.write_batch(message["rows"])
                    if not isinstance(count, int) or isinstance(count, bool) or count < 0:
                        raise ValueError("write_batch must return a non-negative int")
                    _send({"protocol_version": 1, "kind": "ok", "rows": count})
                elif operation == "commit":
                    payload = driver.commit()
                    if not isinstance(payload, dict):
                        raise ValueError("commit must return a dict")
                    _send({"protocol_version": 1, "kind": "ok", "payload": payload})
                    return
                elif operation == "abort":
                    driver.abort()
                    _send({"protocol_version": 1, "kind": "ok"})
                    return
                else:
                    _send({"protocol_version": 1, "kind": "failed", "code": "DOWAI_WRITE_DRIVER_INVALID", "message": "unknown operation"})
                    return
        except BaseException as error:
            _send(_failure(error))
            return

main()
"#;

#[derive(Deserialize)]
struct Reply {
    kind: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    rows: Option<u64>,
    #[serde(default)]
    payload: Option<Value>,
}

/// An open session with an external driver process. Dropping it kills the process.
pub(crate) struct ExternalDriverSession {
    child: Child,
    input: ChildStdin,
    replies: Receiver<std::io::Result<String>>,
    timeout: std::time::Duration,
    columns: Vec<String>,
    staged: u64,
    depth: usize,
}

impl ExternalDriverSession {
    /// Starts the driver, checks its interface and opens the session. Any failure kills the
    /// process before returning, so a rejected session never stays open.
    pub(crate) fn open(
        root: &Path,
        profile: &WorkspaceProfile,
        request: &WriteRequest,
        depth: usize,
        worker: WorkerSettings<'_>,
    ) -> Result<Self> {
        let validated = validate_write_request(request)?;
        let driver_path = driver_file(root, profile)?;
        let mut command = Command::new(worker.interpreter);
        command
            .arg("-I")
            .arg("-c")
            .arg(SHIM)
            .arg(&driver_path)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::python_runtime::apply_worker_environment(&mut command);
        for name in &profile.env {
            let value = std::env::var_os(name).ok_or_else(|| DowaiError::Write {
                code: DRIVER_INVALID,
                message: format!("a declared driver environment variable ({name}) is not set"),
            })?;
            command.env(name, value);
        }
        let mut child = command.spawn().map_err(|_| DowaiError::Write {
            code: DRIVER_INVALID,
            message: "could not start the project interpreter for the driver".to_owned(),
        })?;
        let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DowaiError::Write {
                code: DRIVER_INVALID,
                message: "driver process pipes are unavailable".to_owned(),
            });
        };
        let replies = spawn_reader(output);
        let mut session = Self {
            child,
            input,
            replies,
            timeout: worker.timeout,
            columns: validated.columns.clone(),
            staged: 0,
            depth,
        };
        session.expect_kind("ready")?;
        session.send(json!({
            "protocol_version": PROTOCOL_VERSION,
            "op": "open",
            "profile": profile.params.clone().unwrap_or_else(|| json!({})),
            "table": validated.table,
            "columns": validated.columns,
            "mode": validated.mode.as_str(),
            "keys": validated.keys,
            "confirm_replace": request.confirm_replace,
        }))?;
        session.expect_kind("ok")?;
        Ok(session)
    }

    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    /// Sends one batch. Every row must be a mapping with exactly the declared columns.
    pub(crate) fn stage_rows(&mut self, rows: &[Value]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        for row in rows {
            let complete = row.as_object().is_some_and(|object| {
                object.len() == self.columns.len()
                    && self
                        .columns
                        .iter()
                        .all(|column| object.contains_key(column))
            });
            if !complete {
                return Err(DowaiError::Write {
                    code: crate::external_write::WRITE_ROW_INVALID,
                    message: "every row must have exactly the declared columns".to_owned(),
                });
            }
        }
        self.send(json!({
            "protocol_version": PROTOCOL_VERSION,
            "op": "rows",
            "rows": rows,
        }))?;
        let reply = self.read_reply()?;
        let written = match (reply.kind.as_str(), reply.rows) {
            ("ok", Some(written)) => written,
            _ => return Err(self.driver_error(reply)),
        };
        self.staged += written;
        Ok(written)
    }

    /// Asks the driver to commit and returns its counts. The process ends afterwards.
    pub(crate) fn commit(mut self) -> Result<Value> {
        self.send(json!({ "protocol_version": PROTOCOL_VERSION, "op": "commit" }))?;
        let reply = self.read_reply()?;
        match (reply.kind == "ok", reply.payload.clone()) {
            (true, Some(payload)) => {
                self.finish();
                Ok(json!({ "driver": payload, "staged": self.staged }))
            }
            _ => Err(self.driver_error(reply)),
        }
    }

    /// Best-effort abort: the driver is told to discard, and the process is killed regardless.
    pub(crate) fn abort(mut self) {
        let _ = self.send(json!({ "protocol_version": PROTOCOL_VERSION, "op": "abort" }));
        let _ = self.read_reply();
        self.finish();
    }

    fn expect_kind(&mut self, kind: &str) -> Result<()> {
        let reply = self.read_reply()?;
        if reply.kind == kind {
            Ok(())
        } else {
            Err(self.driver_error(reply))
        }
    }

    fn send(&mut self, message: Value) -> Result<()> {
        let mut line = serde_json::to_vec(&message).map_err(|_| self.protocol())?;
        line.push(b'\n');
        self.input
            .write_all(&line)
            .and_then(|()| self.input.flush())
            .map_err(|_| self.protocol())
    }

    fn read_reply(&mut self) -> Result<Reply> {
        let line = match self.replies.recv_timeout(self.timeout) {
            Ok(Ok(line)) => line,
            Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => return Err(self.protocol()),
            Err(RecvTimeoutError::Timeout) => {
                self.finish();
                return Err(DowaiError::Write {
                    code: DRIVER_TIMEOUT,
                    message: "the external driver did not answer within the timeout".to_owned(),
                });
            }
        };
        let reply: Reply = serde_json::from_str(&line).map_err(|_| self.protocol())?;
        if reply.kind == "failed" {
            return Err(self.driver_error(reply));
        }
        Ok(reply)
    }

    fn driver_error(&self, reply: Reply) -> DowaiError {
        let reported = reply.code.filter(|code| code.starts_with("DOWAI_"));
        let code = if reported.as_deref() == Some(DRIVER_INVALID) {
            DRIVER_INVALID
        } else {
            DRIVER_FAILED
        };
        let detail = reply.message.as_deref().map(sanitize).unwrap_or_default();
        let message = match reported {
            Some(reported) if detail.is_empty() => format!("driver reported {reported}"),
            Some(reported) => format!("driver reported {reported}: {detail}"),
            None if detail.is_empty() => "driver failed".to_owned(),
            None => format!("driver failed: {detail}"),
        };
        DowaiError::Write { code, message }
    }

    fn protocol(&self) -> DowaiError {
        DowaiError::Write {
            code: DRIVER_FAILED,
            message: "the external driver broke the protocol or stopped".to_owned(),
        }
    }

    fn finish(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ExternalDriverSession {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Removes control characters and bounds the length of a driver message before it is reported.
fn sanitize(message: &str) -> String {
    message
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_MESSAGE_CHARS)
        .collect()
}

fn spawn_reader(output: std::process::ChildStdout) -> Receiver<std::io::Result<String>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(output);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if sender.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });
    receiver
}

/// The driver file must be a regular file inside `<project>/drivers/`, with no symbolic link.
fn driver_file(root: &Path, profile: &WorkspaceProfile) -> Result<PathBuf> {
    let refused = |detail: &str| DowaiError::Write {
        code: DRIVER_INVALID,
        message: format!("external driver is not usable: {detail}"),
    };
    let name = profile
        .driver
        .as_deref()
        .ok_or_else(|| refused("no driver is configured"))?;
    let directory = root.join("drivers");
    let candidate = directory.join(name);
    let metadata = std::fs::symlink_metadata(&candidate)
        .map_err(|_| refused("the driver file does not exist"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(refused(
            "the driver must be a regular file, not a link or a directory",
        ));
    }
    let canonical_root = std::fs::canonicalize(root).map_err(|_| refused("project unreadable"))?;
    let canonical_driver =
        std::fs::canonicalize(&candidate).map_err(|_| refused("the driver file is unreadable"))?;
    if !canonical_driver.starts_with(canonical_root.join("drivers")) {
        return Err(refused("the driver resolves outside drivers/"));
    }
    Ok(canonical_driver)
}
