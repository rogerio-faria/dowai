# DOWAI Python SDK

This package currently provides dependency-free Python API types and a
testable `Env` facade. The DOWAI engine is planned as a Rust application; this
package does **not** launch it, implement IPC, execute models, or connect to a
session automatically.

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

Run the dependency-free test suite from this directory:

```powershell
python -m unittest discover -s tests -v
```
