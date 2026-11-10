"""Testable Env facade; this package does not launch or connect to a runtime."""

from collections.abc import Mapping
from contextlib import contextmanager
from typing import Any, Iterator, Protocol

from ._errors import DowaiExecutionError
from ._result import ExecutionMode, RunResult


class Dataset(Protocol):
    """Opaque lazy dataset handle supplied by a bound DOWAI runtime."""

    @property
    def name(self) -> str:
        """Logical dataset name."""


class ExecutionBackend(Protocol):
    """Minimal execution adapter expected by ``Env`` when explicitly bound."""

    def run(
        self,
        target: str,
        *,
        mode: ExecutionMode,
        raise_on_error: bool,
    ) -> RunResult:
        """Execute a target through the host application's execution API."""


class Env:
    """Typed facade for SDK operations, usable with explicit test backends.

    ``Env()`` is intentionally not connected to DOWAI. Local variable and mode
    helpers are useful in tests, but operations requiring the Rust runtime fail
    with ``DOWAI_RUNTIME_UNBOUND`` until a backend is supplied.
    """

    def __init__(
        self,
        *,
        backend: object | None = None,
        mode: ExecutionMode = "automatic",
    ) -> None:
        self._validate_mode(mode)
        self._backend = backend
        self._mode = mode
        self._variables: Mapping[str, Any] = {}

    @property
    def mode(self) -> ExecutionMode:
        """Current execution mode, including any active manual context."""
        return self._mode

    def setVar(self, name: str, value: Any) -> None:
        """Set a session variable in this facade's local context."""
        self._validate_name(name)
        self._variables = {**self._variables, name: value}

    def getVar(self, name: str, default: Any = None) -> Any:
        """Get a local variable, returning ``default`` when it is absent."""
        self._validate_name(name)
        return self._variables.get(name, default)

    def unsetVar(self, name: str) -> bool:
        """Remove a local variable and report whether it existed."""
        self._validate_name(name)
        if name not in self._variables:
            return False
        self._variables = {
            key: value for key, value in self._variables.items() if key != name
        }
        return True

    @contextmanager
    def manual(self) -> Iterator["Env"]:
        """Temporarily use manual mode and restore the previous mode on exit."""
        previous_mode = self._mode
        self._mode = "manual"
        try:
            yield self
        finally:
            self._mode = previous_mode

    def run(
        self,
        target: str,
        *,
        mode: ExecutionMode | None = None,
        raise_on_error: bool = True,
    ) -> RunResult:
        """Execute through an explicitly supplied backend, never a fake runtime."""
        self._validate_target(target)
        selected_mode = self._mode if mode is None else mode
        self._validate_mode(selected_mode)
        backend = self._require_backend("run")
        operation = getattr(backend, "run", None)
        if not callable(operation):
            self._raise_unavailable("run")
        result = operation(
            target,
            mode=selected_mode,
            raise_on_error=raise_on_error,
        )
        if not isinstance(result, RunResult):
            raise TypeError("backend.run() must return a RunResult")
        if raise_on_error:
            result.raise_for_error()
        return result

    def ref(self, name: str) -> Dataset:
        """Resolve a model dataset through a bound runtime backend."""
        self._validate_name(name)
        operation = self._backend_operation("ref")
        return operation(name)

    def source(self, name: str, table: str) -> Dataset:
        """Resolve a source dataset through a bound runtime backend."""
        self._validate_name(name)
        self._validate_name(table)
        operation = self._backend_operation("source")
        return operation(name, table)

    def _backend_operation(self, name: str) -> Any:
        backend = self._require_backend(name)
        operation = getattr(backend, name, None)
        if not callable(operation):
            self._raise_unavailable(name)
        return operation

    def _require_backend(self, operation: str) -> object:
        if self._backend is None:
            raise DowaiExecutionError(
                "DOWAI_RUNTIME_UNBOUND",
                f"Cannot perform {operation}: Env is not bound to a DOWAI runtime.",
                hint="Supply an explicit execution backend or use the DOWAI runtime.",
            )
        return self._backend

    @staticmethod
    def _raise_unavailable(operation: str) -> None:
        raise DowaiExecutionError(
            "DOWAI_OPERATION_UNAVAILABLE",
            f"The bound backend does not support {operation}.",
        )

    @staticmethod
    def _validate_mode(mode: str) -> None:
        if mode not in ("automatic", "manual"):
            raise ValueError("mode must be 'automatic' or 'manual'")

    @staticmethod
    def _validate_name(name: str) -> None:
        if not isinstance(name, str) or not name.strip():
            raise ValueError("name must be a non-empty string")

    @classmethod
    def _validate_target(cls, target: str) -> None:
        cls._validate_name(target)
