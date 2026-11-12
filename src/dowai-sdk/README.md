# DOWAI Python SDK

This package provides dependency-free Python API types, a testable `Env`
facade, and `DowaiClient`, which drives the `dowai` engine binary (AC034).
`Env` does not launch the engine; only `DowaiClient` does, and only on request.

## Engine client (AC034)

`DowaiClient` runs `dowai ... --json` and returns what the engine printed, so
the CLI and Python share one code path. The parity tests compare both paths on
the same project and journal.

```python
from dowai import DowaiClient

client = DowaiClient("/path/to/project", binary="/path/to/dowai")  # or DOWAI_BIN / PATH
plan = client.plan(["consumer.py"])
result = client.run(["consumer.py"], variables={"batch": 7})
if result.failed:
    print(result.error_code)  # stable DOWAI_* code, never a free-text message
    again = client.retry(result.run_id or client.runs(1)["runs"][0]["run_id"])
```

Methods: `plan`, `run`, `retry(run_id, failed_only=False, variables=...)`,
`runs(limit)`, `run_detail(run_id)`, `logs(run_id, node=, level=)`. Engine
refusals in `run`/`retry` come back as a failed `RunResult`; the others raise
`DowaiExecutionError` with the engine's code.

The parity test needs a built binary (`DOWAI_BIN` or `target/debug/dowai`) and
skips otherwise.

```python
from dowai import DowaiExecutionError, Env, RunResult

env = Env()
env.setVar("enabled", True)

with env.manual():
    try:
        env.run("model.sql")
    except DowaiExecutionError as exc:
        if exc.code == "DOWAI_RUNTIME_UNBOUND":
            # This standalone package has no runtime binding.
            pass
```

An unbound `Env` supports local variable storage and mode contexts. `run`,
`ref`, and `source` require an explicit backend and fail with a stable SDK error
when no backend is bound. Backends are intended for integration and test
adapters; they do not imply that the Rust runtime or its policies are present.
The `Dataset` protocol is intentionally opaque so this package does not promise
materialization or transport behavior before the engine contract exists.

Run the test suite from this directory (parity tests need the engine binary):

```powershell
python -m unittest discover -s tests -v
```

## Installation and engine compatibility

The package is versioned in `src/dowai/_version.py` (currently `0.1.0`) and ships `py.typed`.
Requires Python 3.10 or newer; verified on 3.10.22 and 3.12.3.

```bash
python -m venv .venv && .venv/bin/pip install dist/dowai_sdk-0.1.0-py3-none-any.whl
```

`DowaiClient` checks `dowai --version` when it is created. The engine must have the same
`major.minor` as the SDK (pre-1.0 policy: a minor change is breaking). Otherwise the client
raises `DowaiExecutionError` with code `DOWAI_VERSION_INCOMPATIBLE`.
