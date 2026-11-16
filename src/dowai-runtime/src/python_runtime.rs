use crate::dataset_query::FilterRequest;
use crate::executor::DatasetRef;
use crate::external_write::WriteRequest;
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
// Worker -> host, zero or more `{"kind":"write"}` lines (ENT-048, `env.write`), each with
// `operation` ("open", "rows", "commit" or "abort"). `open` carries `target` (profile), `table`,
// `mode`, `keys`, `columns` and `confirm_replace`; `rows` carries `rows` (mappings). The host
// answers each with exactly one line: `write_ok` (`payload`) or `write_failed` (`code`, static).
// A failed write is aborted on the host; the worker sends `abort` whenever it stops early.
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
import csv
import datetime
import decimal
import importlib
import inspect
import itertools
import json
import math
import numbers
import os
import pathlib
import re
import runpy
import secrets
import sys
import types
from collections.abc import Mapping

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

def model_arity(function, error_class):
    """ENT-125: 2 when `model` takes (env, origem), 1 when it takes (env) only."""
    parameters = list(inspect.signature(function).parameters.values())
    if any(parameter.kind is parameter.VAR_POSITIONAL for parameter in parameters):
        return 2
    positional = [
        parameter
        for parameter in parameters
        if parameter.kind in (parameter.POSITIONAL_ONLY, parameter.POSITIONAL_OR_KEYWORD)
    ]
    required = [parameter for parameter in positional if parameter.default is parameter.empty]
    if not positional or len(required) > 2:
        raise error_class(
            "DOWAI_MODEL_SIGNATURE_INVALID", "model() must accept (env) or (env, origem=None)"
        )
    return 2 if len(positional) >= 2 else 1

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

        def _dataset_exchange(self, name, operation, origin=None, **fields):
            if self._closed:
                raise ExecutionError(
                    "DOWAI_DATASET_EXPIRED",
                    "lazy dataset was used after its model finished",
                )
            if origin is None:
                if not isinstance(name, str) or name not in self._dependencies:
                    raise ExecutionError(
                        "DOWAI_DATASET_UNDECLARED",
                        "dataset was not declared as a dependency of this model",
                    )
                message = {"protocol_version": 1, "kind": "dataset", "dataset": name, "operation": operation}
            else:
                # env.source (ENT-042): the host checks the project declaration.
                message = {
                    "protocol_version": 1,
                    "kind": "dataset",
                    "source": origin[0],
                    "table": origin[1],
                    "operation": operation,
                }
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

        def _dataset_request(self, name, operation, origin=None, **fields):
            reply = self._dataset_exchange(name, operation, origin=origin, **fields)
            if reply.get("kind") == "dataset_ok" and isinstance(reply.get("columns"), list):
                return [dict(column) for column in reply["columns"]]
            raise RuntimeError("invalid dataset reply")

        def _dataset_batch(self, name, offset, limit, select, filters, origin=None):
            """Raw batch of a read plus the indexes of predicates the worker must apply."""
            fields = {"offset": offset, "limit": limit}
            if select is not None:
                fields["select"] = list(select)
            if filters:
                fields["filters"] = list(filters)
            reply = self._dataset_exchange(name, "rows", origin=origin, **fields)
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

        def source(self, name, table):
            """`env.source(name, table)`: a table declared by the project (ENT-042)."""
            if not isinstance(name, str) or not name.strip():
                raise ValueError("name must be a non-empty string")
            if not isinstance(table, str) or not table.strip():
                raise ValueError("table must be a non-empty string")
            return Dataset(self, "%s.%s" % (name, table), origin=(name, table))

        @property
        def connections(self):
            """Capability handles for workspace profiles (ENT-043)."""
            return Connections(self)

        def _export_destination(self, path):
            """Asks the host to authorize the destination of an export (ENT-050)."""
            respond({"protocol_version": 1, "kind": "export", "path": path})
            line = _in.readline()
            if not line:
                raise RuntimeError("the DOWAI host closed the export channel")
            reply = json.loads(line)
            if reply.get("protocol_version") != 1:
                raise RuntimeError("invalid export reply")
            if reply.get("kind") == "export_failed":
                raise ExecutionError(
                    str(reply.get("code") or "DOWAI_POLICY_DENIED"),
                    "export destination is not authorized",
                )
            if reply.get("kind") != "export_ok" or not isinstance(reply.get("path"), str):
                raise RuntimeError("invalid export reply")
            return reply["path"]

        def generateCsv(self, data, path, **options):
            """`env.generateCsv(data, path, **options)`: streaming CSV export (ENT-050).

            `data` is a Dataset (read in batches) or an iterable of mappings with the same keys.
            Options: delimiter (default ","), header (default True), null (default ""),
            timezone (IANA name, applied to timezone-aware datetime values only). The file is
            written as UTF-8 to a temporary sibling and renamed after the last row, so a failed
            export never leaves a partial file at the destination. Returns the number of rows.
            """
            check_path(path)
            settings = csv_settings(options)
            target = self._export_destination(path)
            return write_csv(data, target, settings)

        def write(self, data, target, table, mode="append", keys=None, confirm_replace=False):
            """`env.write(data, target, table, mode="append", keys=None, confirm_replace=False)`.

            Writes rows to `table` of the workspace profile `target` (ENT-048). `data` is a Dataset,
            a pandas or Polars DataFrame, a PyArrow table, or an iterable of mappings. `append` adds
            rows; `replace` deletes the destination rows and requires `confirm_replace=True`;
            `upsert` merges by `keys`, which must be a unique key of the destination. The rows are
            written in one destination transaction. The host checks the profile, the policy and the
            schema. Source and destination are not one transaction. Returns the counts.
            """
            return write_destination(self, data, target, table, mode, keys, confirm_replace)

        def _write_exchange(self, operation, **fields):
            message = {"protocol_version": 1, "kind": "write", "operation": operation}
            message.update(fields)
            respond(message)
            line = _in.readline()
            if not line:
                raise RuntimeError("the DOWAI host closed the write channel")
            reply = json.loads(line)
            if reply.get("protocol_version") != 1:
                raise RuntimeError("invalid write reply")
            if reply.get("kind") == "write_failed":
                raise ExecutionError(
                    str(reply.get("code") or "DOWAI_WRITE_FAILED"),
                    "write was not applied to the destination",
                )
            if reply.get("kind") != "write_ok" or not isinstance(reply.get("payload"), dict):
                raise RuntimeError("invalid write reply")
            return reply["payload"]

    def check_path(value):
        if not isinstance(value, str) or not value.strip() or "\x00" in value:
            raise ValueError("path must be a non-empty string without NUL bytes")

    def csv_settings(options):
        allowed = {"delimiter", "header", "null", "timezone"}
        unknown = sorted(set(options) - allowed)
        if unknown:
            raise ValueError("unsupported generateCsv option: %s" % unknown[0])
        delimiter = options.get("delimiter", ",")
        if not isinstance(delimiter, str) or len(delimiter) != 1 or delimiter in '"\r\n':
            raise ValueError("delimiter must be one character, other than quote or line break")
        header = options.get("header", True)
        if not isinstance(header, bool):
            raise ValueError("header must be a bool")
        null = options.get("null", "")
        if not isinstance(null, str):
            raise ValueError("null must be a string")
        zone = None
        timezone = options.get("timezone")
        if timezone is not None:
            if not isinstance(timezone, str) or not timezone.strip():
                raise ValueError("timezone must be an IANA zone name")
            import zoneinfo

            try:
                zone = zoneinfo.ZoneInfo(timezone)
            except Exception:
                raise ValueError("timezone is not a known IANA zone name") from None
        return {"delimiter": delimiter, "header": header, "null": null, "zone": zone}

    def csv_value(value, settings):
        if value is None:
            return settings["null"]
        if isinstance(value, bool):
            return "true" if value else "false"
        if isinstance(value, datetime.datetime):
            if settings["zone"] is not None:
                if value.tzinfo is None:
                    raise ValueError("a naive datetime cannot be converted to the export timezone")
                value = value.astimezone(settings["zone"])
            return value.isoformat()
        if isinstance(value, (datetime.date, datetime.time)):
            return value.isoformat()
        if isinstance(value, (int, float, decimal.Decimal)):
            return str(value)
        if isinstance(value, str):
            return value
        raise ValueError("unsupported value type for CSV export: %s" % type(value).__name__)

    def write_csv(data, target, settings):
        if isinstance(data, Dataset):
            names = [column["name"] for column in data.schema()]
            source = data.iter_rows(batch_size=1000)
        else:
            if isinstance(data, (str, bytes, Mapping)):
                raise ValueError("data must be a Dataset or an iterable of mappings")
            source = iter(data)
            sentinel = object()
            first = next(source, sentinel)
            if first is sentinel:
                names = []
            elif isinstance(first, Mapping):
                names = list(first.keys())
                source = itertools.chain([first], source)
            else:
                raise ValueError("every row must be a mapping")
        width = len(names)

        def fields(row):
            if not isinstance(row, Mapping) or len(row) != width or any(name not in row for name in names):
                raise ValueError("every row must have the same columns")
            return [csv_value(row[name], settings) for name in names]

        directory, base = os.path.split(target)
        temporary = os.path.join(directory, ".%s.%s.dowai-tmp" % (base, secrets.token_hex(8)))
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0)
        descriptor = os.open(temporary, flags, 0o644)
        count = 0
        try:
            with os.fdopen(descriptor, "w", encoding="utf-8", newline="") as handle:
                writer = csv.writer(handle, delimiter=settings["delimiter"], lineterminator="\n")
                if settings["header"] and names:
                    writer.writerow(names)
                for row in source:
                    writer.writerow(fields(row))
                    count += 1
            os.replace(temporary, target)
        except BaseException:
            with contextlib.suppress(OSError):
                os.remove(temporary)
            raise
        return count

    WRITE_MODES = ("append", "replace", "upsert")
    WRITE_BATCH_ROWS = 1000

    def write_value(value):
        if value is None or isinstance(value, (bool, str)):
            return value
        if isinstance(value, numbers.Integral):
            return int(value)
        if isinstance(value, decimal.Decimal):
            if not value.is_finite():
                raise ValueError("a non-finite decimal cannot be written")
            return str(value)
        if isinstance(value, numbers.Real):
            number = float(value)
            if not math.isfinite(number):
                raise ValueError("a non-finite float cannot be written")
            return number
        if isinstance(value, datetime.datetime):
            if value.tzinfo is None:
                raise ValueError("a naive datetime cannot be written; use a timezone-aware value")
            return value.isoformat()
        if isinstance(value, (datetime.date, datetime.time)):
            return value.isoformat()
        raise ValueError("unsupported value type for write: %s" % type(value).__name__)

    def write_source(data):
        if isinstance(data, Dataset):
            names = [column["name"] for column in data.schema()]
            return names, data.iter_rows(batch_size=1000)
        if isinstance(data, (str, bytes, Mapping)):
            raise ValueError("data must be a Dataset, a DataFrame or an iterable of mappings")
        rows = data
        for method in ("to_pylist", "to_dicts"):
            converter = getattr(data, method, None)
            if callable(converter):
                rows = converter()
                break
        else:
            if hasattr(data, "to_dict") and hasattr(data, "columns"):
                rows = data.to_dict("records")
        source = iter(rows)
        sentinel = object()
        first = next(source, sentinel)
        if first is sentinel:
            raise ValueError("there are no rows to write; pass a Dataset to write an empty relation")
        if not isinstance(first, Mapping):
            raise ValueError("every row must be a mapping")
        return list(first.keys()), itertools.chain([first], source)

    def write_destination(environment, data, target, table, mode, keys, confirm_replace):
        if not isinstance(target, str) or not target.strip() or "\x00" in target:
            raise ValueError("target must be a non-empty profile name")
        if not isinstance(table, str) or not table.strip() or "\x00" in table:
            raise ValueError("table must be a non-empty name")
        if mode not in WRITE_MODES:
            raise ValueError("mode must be 'append', 'replace' or 'upsert'")
        if not isinstance(confirm_replace, bool):
            raise ValueError("confirm_replace must be a bool")
        if mode == "upsert":
            if not isinstance(keys, (list, tuple)) or not keys:
                raise ValueError("upsert requires a non-empty list of key columns")
            keys = list(keys)
            if not all(isinstance(key, str) and key.strip() for key in keys):
                raise ValueError("keys must be non-empty strings")
        elif keys is not None:
            raise ValueError("keys are only accepted by upsert")
        if mode == "replace" and not confirm_replace:
            raise ValueError("replace requires confirm_replace=True")
        if mode != "replace" and confirm_replace:
            raise ValueError("confirm_replace only applies to replace")
        names, source = write_source(data)
        names = list(names)
        width = len(names)
        environment._write_exchange(
            "open",
            target=target,
            table=table,
            mode=mode,
            keys=keys,
            columns=names,
            confirm_replace=confirm_replace,
        )
        try:
            batch = []
            for row in source:
                if not isinstance(row, Mapping) or len(row) != width or any(name not in row for name in names):
                    raise ValueError("every row must have the same columns")
                batch.append({name: write_value(row[name]) for name in names})
                if len(batch) >= WRITE_BATCH_ROWS:
                    environment._write_exchange("rows", rows=batch)
                    batch = []
            if batch:
                environment._write_exchange("rows", rows=batch)
            payload = environment._write_exchange("commit")
        except BaseException:
            with contextlib.suppress(Exception):
                environment._write_exchange("abort")
            raise
        return payload

    def check_name(value):
        if not isinstance(value, str) or not value.strip():
            raise ValueError("column must be a non-empty string")
        return value

    def check_batch_size(batch_size):
        if isinstance(batch_size, bool) or not isinstance(batch_size, int) or not 1 <= batch_size <= 10000:
            raise ValueError("batch_size must be an integer between 1 and 10000")

    # ENT-047 (B-09.3): to_arrow/to_pandas/to_polars materialize a dataset. The mapping from
    # PostgreSQL type names (as reported by the schema) to frame types is explicit: any other
    # type is refused with DOWAI_DATASET_TYPE_UNSUPPORTED instead of being guessed.
    DATASET_MAX_ROWS = 1000000
    FRAME_KINDS = {
        "bool": "bool",
        "int2": "int16",
        "int4": "int32",
        "int8": "int64",
        "float4": "float32",
        "float8": "float64",
        "text": "string",
        "varchar": "string",
        "bpchar": "string",
        "name": "string",
        "numeric": "decimal",
        "date": "date",
        "timestamp": "timestamp",
        "timestamptz": "timestamptz",
    }
    TIMESTAMP_TEXT = re.compile(
        r"(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,6}))?"
        r"(?:([+-])(\d{2})(?::?(\d{2}))?(?::?(\d{2}))?)?\Z"
    )

    def check_max_rows(max_rows):
        if isinstance(max_rows, bool) or not isinstance(max_rows, int) or not 1 <= max_rows <= 10000000:
            raise ValueError("max_rows must be an integer between 1 and 10000000")

    def library(module):
        try:
            return importlib.import_module(module)
        except ImportError:
            raise ExecutionError(
                "DOWAI_DATASET_LIBRARY_UNAVAILABLE",
                "%s is not installed in the project virtual environment" % module,
            )

    def parse_timestamp(text, with_zone):
        match = TIMESTAMP_TEXT.match(text)
        if match is None or (match.group(8) is not None) != with_zone:
            raise ExecutionError("DOWAI_DATASET_VALUE_INVALID", "timestamp text is not in the expected form")
        try:
            moment = datetime.datetime(
                *(int(match.group(index)) for index in range(1, 7)),
                int((match.group(7) or "0").ljust(6, "0")),
            )
        except ValueError:
            raise ExecutionError("DOWAI_DATASET_VALUE_INVALID", "timestamp value is out of range")
        if not with_zone:
            return moment
        offset = datetime.timedelta(
            hours=int(match.group(9)),
            minutes=int(match.group(10) or 0),
            seconds=int(match.group(11) or 0),
        )
        if match.group(8) == "-":
            offset = -offset
        # Stored as UTC: the offset in the text is applied, never the host's local zone.
        return (moment - offset).replace(tzinfo=datetime.timezone.utc)

    def frame_value(value, kind):
        if value is None:
            return None
        invalid = ExecutionError("DOWAI_DATASET_VALUE_INVALID", "value does not match its declared type")
        if kind == "bool":
            if not isinstance(value, bool):
                raise invalid
            return value
        if kind in ("int16", "int32", "int64"):
            if isinstance(value, bool) or not isinstance(value, int):
                raise invalid
            return value
        if kind in ("float32", "float64"):
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                raise invalid
            return float(value)
        if kind == "string":
            if not isinstance(value, str):
                raise invalid
            return value
        if kind == "decimal":
            if not isinstance(value, str):
                raise invalid
            try:
                number = decimal.Decimal(value)
            except decimal.InvalidOperation:
                raise invalid
            if not number.is_finite():
                raise ExecutionError("DOWAI_DATASET_VALUE_INVALID", "NaN and infinity have no frame mapping")
            return number
        if kind == "date":
            try:
                return datetime.date.fromisoformat(value)
            except (TypeError, ValueError):
                raise invalid
        if kind in ("timestamp", "timestamptz"):
            if not isinstance(value, str):
                raise invalid
            return parse_timestamp(value, kind == "timestamptz")
        raise invalid

    def decimal_shape(values):
        # Scale keeps the digits PostgreSQL returned (1.50 keeps scale 2); precision covers all values.
        scale = 0
        integer_digits = 0
        for value in values:
            if value is None:
                continue
            parts = value.as_tuple()
            scale = max(scale, -parts.exponent)
            integer_digits = max(integer_digits, len(parts.digits) + parts.exponent)
        precision = max(integer_digits + scale, 1)
        if precision > 38:
            raise ExecutionError("DOWAI_DATASET_TYPE_UNSUPPORTED", "numeric values exceed decimal(38) precision")
        return precision, scale

    def quantized(values, scale):
        quantum = decimal.Decimal(1).scaleb(-scale)
        context = decimal.Context(prec=100)
        return [None if value is None else value.quantize(quantum, context=context) for value in values]

    def arrow_array(pa, kind, values):
        if kind == "decimal":
            precision, scale = decimal_shape(values)
            return pa.array(quantized(values, scale), type=pa.decimal128(precision, scale))
        types = {
            "bool": pa.bool_(),
            "int16": pa.int16(),
            "int32": pa.int32(),
            "int64": pa.int64(),
            "float32": pa.float32(),
            "float64": pa.float64(),
            "string": pa.string(),
            "date": pa.date32(),
            "timestamp": pa.timestamp("us"),
            "timestamptz": pa.timestamp("us", tz="UTC"),
        }
        return pa.array(values, type=types[kind])

    def polars_series(pl, name, kind, values):
        if kind == "decimal":
            precision, scale = decimal_shape(values)
            return pl.Series(name, quantized(values, scale), dtype=pl.Decimal(precision, scale))
        types = {
            "bool": pl.Boolean,
            "int16": pl.Int16,
            "int32": pl.Int32,
            "int64": pl.Int64,
            "float32": pl.Float32,
            "float64": pl.Float64,
            "string": pl.String,
            "date": pl.Date,
            "timestamp": pl.Datetime("us"),
            "timestamptz": pl.Datetime("us", "UTC"),
        }
        return pl.Series(name, values, dtype=types[kind])

    def pandas_series(pd, name, kind, values):
        types = {
            "decimal": object,
            "date": object,
            "bool": "boolean",
            "int16": "Int16",
            "int32": "Int32",
            "int64": "Int64",
            "float32": "Float32",
            "float64": "Float64",
            "string": "string",
            "timestamp": "datetime64[us]",
            "timestamptz": pd.DatetimeTZDtype(unit="us", tz="UTC"),
        }
        return pd.Series(values, name=name, dtype=types[kind])

    def matches(value, predicate):
        # SQL semantics: NULL never equals and never differs (ENT-041).
        if value is None:
            return False
        equal = value == predicate["value"]
        return equal if predicate["op"] == "=" else not equal

    class Connection:
        """Opaque handle to an authorized workspace profile (ENT-043, D05).

        It carries only the profile name and type. It has no URL, host, user, password or
        secret attribute, its repr is safe to log, and it cannot be pickled or mutated.
        """

        __slots__ = ("_name", "_kind")

        def __init__(self, name, kind):
            object.__setattr__(self, "_name", name)
            object.__setattr__(self, "_kind", kind)

        @property
        def name(self):
            return self._name

        @property
        def kind(self):
            return self._kind

        def __setattr__(self, name, value):
            raise AttributeError("connection handles are read-only")

        def __repr__(self):
            return "<DOWAI connection name=%r kind=%r>" % (self._name, self._kind)

        __str__ = __repr__

        def __reduce_ex__(self, protocol):
            raise TypeError("connection handles cannot be pickled")

        def __reduce__(self):
            raise TypeError("connection handles cannot be pickled")

    class Connections:
        """`env.connections`: resolves a profile name through the host policy (ENT-043)."""

        def __init__(self, environment):
            self._environment = environment

        def get(self, name):
            if not isinstance(name, str) or not name.strip():
                raise ValueError("name must be a non-empty string")
            payload = self._environment._command("connection_get", target=name)
            if not isinstance(payload.get("name"), str) or not isinstance(payload.get("kind"), str):
                raise RuntimeError("invalid connection reply")
            return Connection(payload["name"], payload["kind"])

    class Dataset:
        """Lazy relation: select/filter only describe the read; SQL runs it in batches (ENT-041).

        A dataset is bound to the process that created it. Pickling is refused, and any use
        after the model returns fails with DOWAI_DATASET_EXPIRED.
        """

        OPERATORS = ("=", "!=", "<", "<=", ">", ">=", "is_null", "is_not_null")

        def __init__(self, environment, name, select=None, filters=(), origin=None):
            self._environment = environment
            self._name = name
            self._select = select
            self._filters = filters
            self._origin = origin

        def __reduce_ex__(self, protocol):
            raise TypeError("a lazy dataset cannot leave the DOWAI process")

        def __reduce__(self):
            raise TypeError("a lazy dataset cannot leave the DOWAI process")

        @property
        def name(self):
            return self._name

        def schema(self):
            return self._environment._dataset_request(self._name, "schema", origin=self._origin)

        def select(self, *columns):
            if not columns:
                raise ValueError("select needs at least one column")
            for column in columns:
                check_name(column)
            if len(set(columns)) != len(columns):
                raise ValueError("select names the same column twice")
            return Dataset(self._environment, self._name, tuple(columns), self._filters, self._origin)

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
            return Dataset(self._environment, self._name, self._select, self._filters + (predicate,), self._origin)

        def iter_batches(self, batch_size=1000):
            check_batch_size(batch_size)
            return self._iterate_batches(batch_size)

        def iter_rows(self, batch_size=1000):
            return (row for batch in self.iter_batches(batch_size) for row in batch)

        def _frame_columns(self):
            schema = {column["name"]: column["type"] for column in self.schema()}
            names = list(schema) if self._select is None else list(self._select)
            columns = []
            for name in names:
                if name not in schema:
                    raise ExecutionError("DOWAI_DATASET_COLUMN_UNKNOWN", "column is not in the dataset schema")
                kind = FRAME_KINDS.get(schema[name])
                if kind is None:
                    raise ExecutionError(
                        "DOWAI_DATASET_TYPE_UNSUPPORTED",
                        "column type has no explicit frame mapping; select other columns",
                    )
                columns.append((name, kind))
            return columns

        def _materialize(self, max_rows):
            check_max_rows(max_rows)
            columns = self._frame_columns()
            values = {name: [] for name, _ in columns}
            total = 0
            for batch in self._iterate_batches(1000):
                for row in batch:
                    total += 1
                    if total > max_rows:
                        raise ExecutionError(
                            "DOWAI_DATASET_TOO_LARGE",
                            "dataset exceeds max_rows; filter or select fewer rows",
                        )
                    for name, kind in columns:
                        values[name].append(frame_value(row[name], kind))
            return columns, values

        def to_arrow(self, max_rows=DATASET_MAX_ROWS):
            """Materialize as a pyarrow.Table (ENT-047). Memory: bounded by max_rows."""
            pa = library("pyarrow")
            columns, values = self._materialize(max_rows)
            arrays = [arrow_array(pa, kind, values[name]) for name, kind in columns]
            return pa.Table.from_arrays(arrays, names=[name for name, _ in columns])

        def to_pandas(self, max_rows=DATASET_MAX_ROWS):
            """Materialize as a pandas.DataFrame with nullable dtypes (ENT-047)."""
            pd = library("pandas")
            columns, values = self._materialize(max_rows)
            return pd.DataFrame(
                {name: pandas_series(pd, name, kind, values[name]) for name, kind in columns}
            )

        def to_polars(self, max_rows=DATASET_MAX_ROWS):
            """Materialize as a polars.DataFrame (ENT-047)."""
            pl = library("polars")
            columns, values = self._materialize(max_rows)
            return pl.DataFrame(
                [polars_series(pl, name, kind, values[name]) for name, kind in columns]
            )

        def _iterate_batches(self, batch_size):
            offset = 0
            while True:
                raw, residual = self._environment._dataset_batch(
                    self._name, offset, batch_size, self._select, self._filters, origin=self._origin
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
    origin_name = request.get("origin")
    if origin_name is not None and (not isinstance(origin_name, str) or not origin_name.strip()):
        raise RuntimeError("invalid origin")
    if origin_name:
        dependencies.add(origin_name)
    environment = Env(request["variables"], dependencies, mode)
    try:
        with contextlib.redirect_stdout(sys.stderr):
            namespace = runpy.run_path(sys.argv[1], run_name="__main__")
            model = namespace.get("model")
            if not callable(model):
                raise RuntimeError()
            if model_arity(model, ExecutionError) == 2:
                origin = environment.ref(origin_name) if origin_name else None
                result = model(environment, origin)
            elif origin_name:
                raise ExecutionError(
                    "DOWAI_MODEL_SIGNATURE_INVALID",
                    "run() passes an origin, so model() must accept (env, origem)",
                )
            else:
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
    /// Parent SQL model whose materialized relation is passed as `origem` (ENT-125).
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<&'a str>,
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
    keys: Option<Vec<String>>,
    #[serde(default)]
    columns: Option<Vec<String>>,
    #[serde(default)]
    confirm_replace: Option<bool>,
    #[serde(default)]
    rows: Option<Vec<Value>>,
    /// `env.source` (ENT-042): the declared source and its table, instead of `dataset`.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    table: Option<String>,
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
    /// `export` (ENT-050): the destination requested by `env.generateCsv`.
    #[serde(default)]
    path: Option<String>,
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
        target: DatasetRef,
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
    /// Destination of `env.generateCsv` (ENT-050), as requested by the model.
    Export {
        path: String,
    },
    /// One operation of `env.write` (ENT-048).
    Write(WriteRequest),
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
    /// Absolute target path authorized for the export.
    Export(Result<String>),
    /// Payload of a write operation (ENT-048), or the refusal.
    Write(Result<Value>),
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
struct ExportReply<'a> {
    protocol_version: u32,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
}

#[derive(Serialize)]
struct WriteReply<'a> {
    protocol_version: u32,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
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
    /// Modelo SQL pai cujo resultado é a `origem` (ENT-125); `None` quando não há pai.
    pub origin: Option<&'a str>,
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
        origin: invocation.origin,
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
        if matches!(
            message.kind.as_str(),
            "call" | "dataset" | "command" | "export" | "write"
        ) {
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
                // ENT-125: the worker's own contract check, raised before `model()` runs. It has
                // no invocation id, so a model cannot forge it through a re-raised call.
                (Some("DOWAI_MODEL_SIGNATURE_INVALID"), None) => Some(DowaiError::ModelContract(
                    "model() must accept (env) or (env, origem=None), matching run() arguments"
                        .to_owned(),
                )),
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
                let Some(operation) = message.operation else {
                    return Err(protocol_error());
                };
                // Exactly one origin: a plan model (`dataset`) or a declared source
                // (`source` and `table`). Any other combination is a protocol error.
                let target = match (message.dataset, message.source, message.table) {
                    (Some(dataset), None, None) => DatasetRef::Model(dataset),
                    (None, Some(source), Some(table)) => DatasetRef::Source { source, table },
                    _ => return Err(protocol_error()),
                };
                let HostReply::Dataset(result) = on_request(HostRequest::Dataset {
                    target,
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
            "write" => {
                let Some(operation) = message.operation else {
                    return Err(protocol_error());
                };
                let HostReply::Write(result) = on_request(HostRequest::Write(WriteRequest {
                    operation,
                    target: message.target,
                    table: message.table,
                    mode: message.mode,
                    keys: message.keys,
                    columns: message.columns,
                    confirm_replace: message.confirm_replace.unwrap_or(false),
                    rows: message.rows.unwrap_or_default(),
                })) else {
                    return Err(protocol_error());
                };
                let mut bytes = match &result {
                    Ok(payload) => serde_json::to_vec(&WriteReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "write_ok",
                        payload: Some(payload),
                        code: None,
                    }),
                    Err(error) => serde_json::to_vec(&WriteReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "write_failed",
                        payload: None,
                        code: Some(error.code()),
                    }),
                }
                .map_err(|_| protocol_error())?;
                // A payload that does not fit the protocol is refused, not truncated.
                if bytes.len() > MAX_IPC_BYTES {
                    bytes = serde_json::to_vec(&WriteReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "write_failed",
                        payload: None,
                        code: Some("DOWAI_EXECUTION_FAILED"),
                    })
                    .map_err(|_| protocol_error())?;
                }
                bytes.push(b'\n');
                self.replies.send(bytes).map_err(|_| protocol_error())?;
                Ok(())
            }
            "export" => {
                let Some(path) = message.path else {
                    return Err(protocol_error());
                };
                let HostReply::Export(result) = on_request(HostRequest::Export { path }) else {
                    return Err(protocol_error());
                };
                // The reply carries only the authorized path or a static code, never host detail.
                let bytes = match &result {
                    Ok(resolved) => serde_json::to_vec(&ExportReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "export_ok",
                        path: Some(resolved.as_str()),
                        code: None,
                    }),
                    Err(error) => serde_json::to_vec(&ExportReply {
                        protocol_version: IPC_PROTOCOL_VERSION,
                        kind: "export_failed",
                        path: None,
                        code: Some(error.code()),
                    }),
                }
                .map_err(|_| protocol_error())?;
                let mut bytes = bytes;
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

    /// Runs `source` as the model of a worker with `origin` (ENT-125) and returns its variables.
    fn run_model_source(source: &str, origin: Option<&str>) -> Result<BTreeMap<String, Value>> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "dowai-model-contract-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create model directory");
        // The worker only runs inside a virtual environment (sys.prefix != sys.base_prefix).
        let venv = directory.join("venv");
        let created = Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .status()
            .expect("python3 must be available to create the test venv");
        assert!(created.success(), "test venv creation failed");
        let interpreter = if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        };
        let model_path = directory.join("model.py");
        std::fs::write(&model_path, source).expect("write model");
        let cancel = CancellationToken::new();
        let dependencies: Vec<String> = Vec::new();
        let result = execute_model(
            &ModelInvocation {
                project_root: &directory,
                interpreter: &interpreter,
                deadline: Instant::now() + Duration::from_secs(60),
                model_path: &model_path,
                dependencies: &dependencies,
                mode: "automatic",
                cancel: &cancel,
                memory_bytes: None,
                origin,
            },
            &BTreeMap::new(),
            &mut |_request| unreachable!("the contract tests do not request host data"),
        );
        let _ = std::fs::remove_dir_all(&directory);
        result
    }

    #[test]
    fn model_with_one_parameter_runs_without_origin() {
        let variables = run_model_source("def model(env):\n    env.setVar('ok', True)\n", None)
            .expect("legacy signature should run");
        assert_eq!(variables.get("ok"), Some(&Value::Bool(true)));
    }

    #[test]
    fn model_with_origem_receives_none_when_not_called_from_a_sql_parent() {
        let variables = run_model_source(
            "def model(env, origem=None):\n    env.setVar('is_none', origem is None)\n",
            None,
        )
        .expect("origem=None should be passed");
        assert_eq!(variables.get("is_none"), Some(&Value::Bool(true)));
    }

    #[test]
    fn model_with_origem_receives_a_lazy_dataset_when_origin_is_given() {
        let variables = run_model_source(
            "def model(env, origem=None):\n    env.setVar('is_none', origem is None)\n",
            Some("arq"),
        )
        .expect("origem should be a dataset");
        assert_eq!(variables.get("is_none"), Some(&Value::Bool(false)));
    }

    #[test]
    fn legacy_model_refuses_an_origin_instead_of_ignoring_it() {
        let error = run_model_source("def model(env):\n    pass\n", Some("arq"))
            .expect_err("a model without origem must not silently drop the parent");
        assert_eq!(error.code(), "DOWAI_MODEL_SIGNATURE_INVALID");
    }

    #[test]
    fn model_with_too_many_required_parameters_is_refused() {
        let error = run_model_source("def model(env, a, b):\n    pass\n", None)
            .expect_err("three required parameters are not part of the contract");
        assert_eq!(error.code(), "DOWAI_MODEL_SIGNATURE_INVALID");
    }

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
