"""Typed execution result shared by SDK clients and test doubles."""

from dataclasses import dataclass
from typing import Any, Literal

from ._errors import DowaiExecutionError

ExecutionStatus = Literal[
    "SUCCEEDED",
    "FAILED",
    "CANCELLED",
    "SKIPPED",
    "RUNNING",
]
ExecutionMode = Literal["automatic", "manual"]


@dataclass(frozen=True)
class RunResult:
    """Result envelope; optional metadata remains unset when unavailable."""

    status: ExecutionStatus
    schema_version: int = 2
    session_id: str | None = None
    run_id: str | None = None
    invocation_id: str | None = None
    execution_mode: ExecutionMode | None = None
    duration_ms: int | None = None
    nodes: tuple[Any, ...] = ()
    warnings: tuple[str, ...] = ()
    error_code: str | None = None
    message: str | None = None

    def __post_init__(self) -> None:
        if self.status not in (
            "SUCCEEDED",
            "FAILED",
            "CANCELLED",
            "SKIPPED",
            "RUNNING",
        ):
            raise ValueError("status is not a supported execution status")
        if (
            isinstance(self.schema_version, bool)
            or not isinstance(self.schema_version, int)
            or self.schema_version < 1
        ):
            raise ValueError("schema_version must be positive")
        if self.duration_ms is not None and (
            isinstance(self.duration_ms, bool)
            or not isinstance(self.duration_ms, int)
            or self.duration_ms < 0
        ):
            raise ValueError("duration_ms must be a non-negative integer")
        if self.execution_mode not in (None, "automatic", "manual"):
            raise ValueError("execution_mode must be 'automatic' or 'manual'")
        if not isinstance(self.nodes, tuple):
            object.__setattr__(self, "nodes", tuple(self.nodes))
        if not isinstance(self.warnings, tuple):
            object.__setattr__(self, "warnings", tuple(self.warnings))
        if any(not isinstance(warning, str) for warning in self.warnings):
            raise ValueError("warnings must contain only strings")

    @property
    def failed(self) -> bool:
        """Whether the execution completed with a failure status."""
        return self.status == "FAILED"

    def raise_for_error(self) -> None:
        """Raise the public execution exception when this result failed."""
        if self.failed:
            raise DowaiExecutionError(
                self.error_code or "DOWAI_EXECUTION_FAILED",
                self.message or "DOWAI execution failed",
                run_id=self.run_id,
                invocation_id=self.invocation_id,
            )
