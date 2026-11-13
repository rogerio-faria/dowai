use crate::dataset_query::FilterRequest;
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
// Worker -> host, zero or more `{"kind":"dataset"}` lines (AC002, `env.ref`),
// each with `dataset` and `operation` ("schema"). The host answers each with
// exactly one line: `dataset_ok` (`columns`: [{name, type}]) or `dataset_failed`
// (`code`). Only datasets declared as dependencies of the model are served, and
// only from SQL models already committed by the run.
//
// Worker -> host, zero or more `{"kind":"command"}` lines (ENT-040, `env.plan` and
// `env.validate`), each with `operation` ("plan" or "validate") and, for "plan", `target`
// and `mode`; both carry the caller's `variables`. The host answers with exactly one line:
// `command_ok` (`payload`: the same JSON the CLI prints for the operation) or
// `command_failed` (`code`, static). `call_ok` also carries `run_id`, `session_id`,
// `execution_mode` and `duration_ms` (additive) so `env.run` returns the §6.3 fields.
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

class EngineResult:
    # Mirrors the SDK result contract (plano §6.3) for nested `env.run` calls.
    def __init__(self, fields):
        self.schema_version = 2
        self.session_id = fields.get("session_id")
        self.run_id = fields.get("run_id")
        self.invocation_id = fields.get("invocation_id")
        self.status = fields.get("status") or "SUCCEEDED"
        self.execution_mode = fields.get("execution_mode")
        self.duration_ms = fields.get("duration_ms")
        self.nodes = ()
        self.warnings = ()
        self.error_code = None
        self.message = "Completed"

    @property
    def failed(self):
        return self.status != "SUCCEEDED"

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
            self._closed = False

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
                return EngineResult(reply)
            if reply.get("kind") == "call_failed":
                raise ExecutionError(
                    str(reply.get("code") or "DOWAI_EXECUTION_FAILED"),
                    str(reply.get("message") or "Nested model invocation failed"),
                    invocation_id=reply.get("invocation_id"),
                )
            raise RuntimeError("invalid nested call reply")

        def _command(self, operation, target=None, mode=None):
            message = {"protocol_version": 1, "kind": "command", "operation": operation, "variables": self._variables}
            if target is not None:
                message["target"] = target
            if mode is not None:
                message["mode"] = mode
            json.dumps(self._variables)
            respond(message)
            line = _in.readline()
            if not line:
                raise RuntimeError("the DOWAI host closed the command channel")
            reply = json.loads(line)
            if reply.get("protocol_version") != 1:
                raise RuntimeError("invalid command reply")
            if reply.get("kind") == "command_ok" and isinstance(reply.get("payload"), dict):
                return reply["payload"]
            if reply.get("kind") == "command_failed":
                raise ExecutionError(
                    str(reply.get("code") or "DOWAI_EXECUTION_FAILED"),
                    "engine command was rejected: " + operation,
                )
            raise RuntimeError("invalid command reply")

        def plan(self, target, *, mode=None):
            if not isinstance(target, str) or not target.strip():
                raise ValueError("target must be a non-empty string")
            selected = self._mode if mode is None else mode
            if selected not in ("automatic", "manual"):
                raise ValueError("mode must be 'automatic' or 'manual'")
            return self._command("plan", target=target, mode=selected)

        def validate(self):
            return self._command("validate")

        def _dataset_exchange(self, name, operation, **fields):
            if self._closed:
                raise ExecutionError(
                    "DOWAI_DATASET_EXPIRED",
                    "lazy dataset was used after its model finished",
                )
            if not isinstance(name, str) or name not in self._dependencies:
                raise ExecutionError(
                    "DOWAI_DATASET_UNDECLARED",
                    "dataset was not declared as a dependency of this model",
                )
            message = {"protocol_version": 1, "kind": "dataset", "dataset": name, "operation": operation}
            message.update(fields)
            respond(message)
            line = _in.readline()
            if not line:
                raise RuntimeError("the DOWAI host closed the dataset channel")
            reply = json.loads(line)
            if reply.get("protocol_version") != 1:
                raise RuntimeError("invalid dataset reply")
            if reply.get("kind") == "dataset_failed":
                raise ExecutionError(
                    str(reply.get("code") or "DOWAI_DATASET_UNAVAILABLE"),
                    "dataset is not available to this model",
                )
            return reply

        def _dataset_request(self, name, operation, **fields):
            reply = self._dataset_exchange(name, operation, **fields)
            if reply.get("kind") == "dataset_ok" and isinstance(reply.get("columns"), list):
                return [dict(column) for column in reply["columns"]]
            raise RuntimeError("invalid dataset reply")

        def _dataset_batch(self, name, offset, limit, select, filters):
            """Raw batch of a read plus the indexes of predicates the worker must apply."""
            fields = {"offset": offset, "limit": limit}
            if select is not None:
                fields["select"] = list(select)
            if filters:
                fields["filters"] = list(filters)
            reply = self._dataset_exchange(name, "rows", **fields)
            if (
                reply.get("kind") != "dataset_rows"
                or not isinstance(reply.get("columns"), list)
                or not isinstance(reply.get("rows"), list)
            ):
                raise RuntimeError("invalid dataset reply")
            residual = reply.get("residual", [])
            if not isinstance(residual, list) or not all(
                isinstance(index, int) and not isinstance(index, bool) and 0 <= index < len(filters)
                for index in residual
            ):
                raise RuntimeError("invalid dataset reply")
            names = reply["columns"]
            return [dict(zip(names, row)) for row in reply["rows"]], residual

        def ref(self, name):
            if not isinstance(name, str) or not name.strip():
                raise ValueError("name must be a non-empty string")
            return Dataset(self, name)

        def _unsupported(self, *args, **kwargs):
            raise NotImplementedError()

        source = _unsupported
        write = _unsupported
        generateCsv = _unsupported

    def check_name(value):
        if not isinstance(value, str) or not value.strip():
            raise ValueError("column must be a non-empty string")
        return value

    def check_batch_size(batch_size):
        if isinstance(batch_size, bool) or not isinstance(batch_size, int) or not 1 <= batch_size <= 10000:
            raise ValueError("batch_size must be an integer between 1 and 10000")

    def matches(value, predicate):
        # SQL semantics: NULL never equals and never differs (ENT-041).
        if value is None:
            return False
        equal = value == predicate["value"]
        return equal if predicate["op"] == "=" else not equal

    class Dataset:
        """Lazy relation: select/filter only describe the read; SQL runs it in batches (ENT-041).

        A dataset is bound to the process that created it. Pickling is refused, and any use
        after the model returns fails with DOWAI_DATASET_EXPIRED.
        """

        OPERATORS = ("=", "!=", "<", "<=", ">", ">=", "is_null", "is_not_null")

        def __init__(self, environment, name, select=None, filters=()):
            self._environment = environment
            self._name = name
            self._select = select
            self._filters = filters

        def __reduce_ex__(self, protocol):
            raise TypeError("a lazy dataset cannot leave the DOWAI process")

        def __reduce__(self):
            raise TypeError("a lazy dataset cannot leave the DOWAI process")

        @property
        def name(self):
            return self._name

        def schema(self):
            return self._environment._dataset_request(self._name, "schema")

        def select(self, *columns):
            if not columns:
                raise ValueError("select needs at least one column")
            for column in columns:
                check_name(column)
            if len(set(columns)) != len(columns):
                raise ValueError("select names the same column twice")
            return Dataset(self._environment, self._name, tuple(columns), self._filters)

        def filter(self, column, op, value=None):
            check_name(column)
            if op not in Dataset.OPERATORS:
                raise ValueError("unsupported filter operator")
            if op in ("is_null", "is_not_null"):
                if value is not None:
                    raise ValueError("is_null and is_not_null take no value")
            else:
                if value is None or not isinstance(value, (bool, int, float, str)):
                    raise ValueError("filter value must be bool, int, float or str")
                if isinstance(value, float) and (value != value or value in (float("inf"), float("-inf"))):
                    raise ValueError("filter value must be finite")
            predicate = {"column": column, "op": op, "value": value}
            return Dataset(self._environment, self._name, self._select, self._filters + (predicate,))

        def iter_batches(self, batch_size=1000):
            check_batch_size(batch_size)
            return self._iterate_batches(batch_size)

        def iter_rows(self, batch_size=1000):
            return (row for batch in self.iter_batches(batch_size) for row in batch)

        def _iterate_batches(self, batch_size):
            offset = 0
            while True:
                raw, residual = self._environment._dataset_batch(
                    self._name, offset, batch_size, self._select, self._filters
                )
                kept = [
                    row
                    for row in raw
                    if all(matches(row[self._filters[index]["column"]], self._filters[index]) for index in residual)
                ]
                if self._select is not None:
                    kept = [{name: row[name] for name in self._select} for row in kept]
                if kept:
                    yield kept
                # The end of the relation is the raw batch length, not the kept length.
                if len(raw) < batch_size:
                    return
                offset += batch_size

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
    finally:
        environment._closed = True
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
    #[serde(default)]
    dataset: Option<String>,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    select: Option<Vec<String>>,
    #[serde(default)]
    filters: Option<Vec<FilterRequest>>,
}

/// One column of a dataset, as the worker sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DatasetColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub data_type: String,
}

/// A request from the worker: a nested call, a dataset operation or an engine command.
pub(crate) enum HostRequest {
    Call(ChildCall),
    Dataset {
        dataset: String,
        operation: String,
        offset: Option<u64>,
        limit: Option<u32>,
        select: Option<Vec<String>>,
        filters: Vec<FilterRequest>,
    },
    Command {
        operation: String,
        target: Option<String>,
        mode: Option<String>,
        variables: BTreeMap<String, Value>,
    },
}

/// Successful answer of a dataset operation.
pub(crate) enum DatasetAnswer {
    Columns(Vec<DatasetColumn>),
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        /// Predicates (indexes into the request) the worker applies after fetching (ENT-041).
        residual: Vec<usize>,
    },
}

/// The host's answer to a [`HostRequest`]; the variant matches the request.
pub(crate) enum HostReply {
    Call(ChildOutcome),
    Dataset(Result<DatasetAnswer>),
    Command(Result<Value>),
}

#[derive(Serialize)]
struct DatasetRowsReply<'a> {
    protocol_version: u32,
    kind: &'a str,
    columns: &'a [String],
    rows: &'a [Vec<Value>],
    residual: &'a [usize],
}

#[derive(Serialize)]
struct DatasetReply<'a> {
    protocol_version: u32,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    columns: Option<&'a [DatasetColumn]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_mode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
}

#[derive(Serialize)]
struct CommandReply<'a> {
    protocol_version: u32,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
}

/// A nested `env.run()` request issued by a running model.
pub(crate) struct ChildCall {
    pub target: String,
    pub mode: Option<String>,
    pub variables: BTreeMap<String, Value>,
}

/// Identifiers and timing of a successful nested call (ENT-040, `env.run` result).
/// Never carries variable values.
pub(crate) struct CallDetails {
    pub run_id: String,
    pub session_id: Option<String>,
    pub execution_mode: String,
    pub duration_ms: u64,
}

/// Result of a nested call, as decided by the host.
pub(crate) struct ChildOutcome {
    pub invocation_id: String,
    pub result: Result<BTreeMap<String, Value>>,
    /// Present only when `result` is `Ok`.
    pub details: Option<CallDetails>,
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
    /// `RLIMIT_AS` do worker (ENT-038, D06); `None` quando o limite não se aplica.
    pub memory_bytes: Option<u64>,
}

type RequestHandler<'a> = dyn FnMut(HostRequest) -> HostReply + 'a;

pub(crate) fn execute_model(
    invocation: &ModelInvocation<'_>,
    variables: &BTreeMap<String, Value>,
    on_request: &mut RequestHandler<'_>,
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
    configure_memory_limit(&mut command, invocation.memory_bytes);
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
    let driven = supervision.drive(&mut child, on_request);
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
        if matches!(message.kind.as_str(), "call" | "dataset" | "command") {
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
        on_request: &mut RequestHandler<'_>,
    ) -> Result<std::process::ExitStatus> {
        loop {
            // Checked first so a cancelled run never serves further nested calls; the
            // caller terminates the whole process tree on any error.
            self.cancel.check()?;
            while let Ok(line) = self.messages.try_recv() {
                self.handle(line, on_request)?;
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
                Ok(line) => self.handle(line, on_request)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => thread::sleep(POLL_INTERVAL),
            }
        }
    }

    fn handle(
        &mut self,
        line: io::Result<Vec<u8>>,
        on_request: &mut RequestHandler<'_>,
    ) -> Result<()> {
        let line = line.map_err(|_| protocol_error())?;
        if line.len() > MAX_IPC_BYTES || self.final_message.is_some() {
            return Err(protocol_error());
        }
        let message = parse_message(&line)?;
        match message.kind.as_str() {
            "call" => {
                let (Some(target), Some(variables)) = (message.target, message.variables) else {
                    return Err(protocol_error());
                };
                let HostReply::Call(outcome) = on_request(HostRequest::Call(ChildCall {
                    target,
                    mode: message.mode,
                    variables,
                })) else {
                    return Err(protocol_error());
                };
                let details = outcome.details.as_ref();
                let reply = match &outcome.result {
                    Ok(variables) => CallReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "call_ok",
                        invocation_id: &outcome.invocation_id,
                        variables: Some(variables),
                        code: None,
                        message: None,
                        run_id: details.map(|d| d.run_id.as_str()),
                        session_id: details.and_then(|d| d.session_id.as_deref()),
                        execution_mode: details.map(|d| d.execution_mode.as_str()),
                        duration_ms: details.map(|d| d.duration_ms),
                        status: details.map(|_| "SUCCEEDED"),
                    },
                    Err(error) => CallReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "call_failed",
                        invocation_id: &outcome.invocation_id,
                        variables: None,
                        code: Some(error.code()),
                        message: Some(worker_visible_message(error.code())),
                        run_id: None,
                        session_id: None,
                        execution_mode: None,
                        duration_ms: None,
                        status: None,
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
            "dataset" => {
                let (Some(dataset), Some(operation)) = (message.dataset, message.operation) else {
                    return Err(protocol_error());
                };
                let HostReply::Dataset(result) = on_request(HostRequest::Dataset {
                    dataset,
                    operation,
                    offset: message.offset,
                    limit: message.limit,
                    select: message.select,
                    filters: message.filters.unwrap_or_default(),
                }) else {
                    return Err(protocol_error());
                };
                let mut bytes = match &result {
                    Ok(DatasetAnswer::Columns(columns)) => serde_json::to_vec(&DatasetReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "dataset_ok",
                        columns: Some(columns),
                        code: None,
                    }),
                    Ok(DatasetAnswer::Rows {
                        columns,
                        rows,
                        residual,
                    }) => serde_json::to_vec(&DatasetRowsReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "dataset_rows",
                        columns,
                        rows,
                        residual,
                    }),
                    Err(error) => serde_json::to_vec(&DatasetReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "dataset_failed",
                        columns: None,
                        code: Some(error.code()),
                    }),
                }
                .map_err(|_| protocol_error())?;
                // A batch that does not fit the protocol is refused, not truncated.
                if bytes.len() > MAX_IPC_BYTES {
                    bytes = serde_json::to_vec(&DatasetReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "dataset_failed",
                        columns: None,
                        code: Some(crate::executor::DATASET_LIMIT),
                    })
                    .map_err(|_| protocol_error())?;
                }
                bytes.push(b'\n');
                self.replies.send(bytes).map_err(|_| protocol_error())?;
                Ok(())
            }
            "command" => {
                let Some(operation) = message.operation else {
                    return Err(protocol_error());
                };
                let Some(variables) = message.variables else {
                    return Err(protocol_error());
                };
                let HostReply::Command(result) = on_request(HostRequest::Command {
                    operation,
                    target: message.target,
                    mode: message.mode,
                    variables,
                }) else {
                    return Err(protocol_error());
                };
                let mut bytes = match &result {
                    Ok(payload) => serde_json::to_vec(&CommandReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "command_ok",
                        payload: Some(payload),
                        code: None,
                    }),
                    Err(error) => serde_json::to_vec(&CommandReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "command_failed",
                        payload: None,
                        code: Some(error.code()),
                    }),
                }
                .map_err(|_| protocol_error())?;
                // A payload that does not fit the protocol is refused, not truncated.
                if bytes.len() > MAX_IPC_BYTES {
                    bytes = serde_json::to_vec(&CommandReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "command_failed",
                        payload: None,
                        code: Some("DOWAI_EXECUTION_FAILED"),
                    })
                    .map_err(|_| protocol_error())?;
                }
                bytes.push(b'\n');
                self.replies.send(bytes).map_err(|_| protocol_error())?;
                Ok(())
            }
            _ => {
                self.final_message = Some(message);
                Ok(())
            }
        }
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

pub(crate) fn apply_worker_environment(command: &mut Command) {
    command.env_clear();
    for name in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("PYTHONDONTWRITEBYTECODE", "1");
}

/// Aplica `RLIMIT_AS` ao worker antes do `exec` (Linux, ENT-038). O limite é gravado como
/// soft e hard, sem ultrapassar o hard limit herdado, porque reduzir o hard é irreversível.
#[cfg(unix)]
fn configure_memory_limit(command: &mut Command, memory_bytes: Option<u64>) {
    use std::os::unix::process::CommandExt;
    let Some(requested) = memory_bytes else {
        return;
    };
    // SAFETY: the closure runs between fork and exec and only calls getrlimit/setrlimit,
    // which are async-signal-safe and allocate nothing.
    unsafe {
        command.pre_exec(move || {
            let mut current = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_AS, &mut current) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut target = requested as libc::rlim_t;
            if current.rlim_max != libc::RLIM_INFINITY && target > current.rlim_max {
                target = current.rlim_max;
            }
            let limit = libc::rlimit {
                rlim_cur: target,
                rlim_max: target,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_memory_limit(_command: &mut Command, _memory_bytes: Option<u64>) {}

#[cfg(unix)]
pub(crate) fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
pub(crate) fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
pub(crate) struct ProcessTree {
    process_group: i32,
}

#[cfg(unix)]
pub(crate) fn attach_process_tree(child: &mut Child) -> Result<ProcessTree> {
    let process_group = i32::try_from(child.id())
        .map_err(|_| python_failure("Invalid Python worker process identifier"))?;
    Ok(ProcessTree { process_group })
}

#[cfg(unix)]
pub(crate) fn terminate_process_tree(tree: &ProcessTree, child: &mut Child) -> Result<()> {
    terminate_group(tree)?;
    terminate_child(child)
}

#[cfg(unix)]
pub(crate) fn terminate_descendants(tree: &ProcessTree) -> Result<()> {
    terminate_group(tree)
}

#[cfg(windows)]
pub(crate) struct ProcessTree(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
pub(crate) fn attach_process_tree(child: &mut Child) -> Result<ProcessTree> {
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
pub(crate) fn terminate_process_tree(tree: &ProcessTree, _child: &mut Child) -> Result<()> {
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
pub(crate) fn terminate_descendants(tree: &ProcessTree) -> Result<()> {
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
pub(crate) struct ProcessTree;

#[cfg(not(any(unix, windows)))]
pub(crate) fn attach_process_tree(_child: &mut Child) -> Result<ProcessTree> {
    Ok(ProcessTree)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn terminate_process_tree(_tree: &ProcessTree, child: &mut Child) -> Result<()> {
    child
        .kill()
        .map_err(|_| python_failure("Could not terminate the timed-out Python worker"))?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn terminate_descendants(_tree: &ProcessTree) -> Result<()> {
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
