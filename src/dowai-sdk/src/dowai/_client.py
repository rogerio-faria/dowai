"""Engine client for Python: every operation is the same `dowai ... --json` command line.

AC034 requires that an operation gives the same plan, policy, status and identifiers from the
CLI and from Python. The client therefore does not re-implement anything: it runs the engine
binary with `--json` and returns what the engine printed. The parity test compares both paths.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

from ._errors import DowaiExecutionError
from ._result import RunResult
from ._version import __version__ as _SDK_VERSION

_SDK_SERIES = ".".join(_SDK_VERSION.split(".")[:2])

_ENVELOPE_STATUSES = ("SUCCEEDED", "FAILED", "CANCELLED")


class DowaiClient:
    """Drives one project through the `dowai` executable.

    `binary` defaults to the `DOWAI_BIN` environment variable, then to `dowai` on `PATH`.
    """

    def __init__(
        self,
        project_dir: str | os.PathLike[str],
        *,
        binary: str | os.PathLike[str] | None = None,
        timeout_seconds: float | None = None,
    ) -> None:
        resolved = str(binary or os.environ.get("DOWAI_BIN") or shutil.which("dowai") or "")
        if not resolved or shutil.which(resolved) is None:
            raise DowaiExecutionError(
                "DOWAI_CLI_NOT_FOUND",
                "The dowai executable was not found; set DOWAI_BIN or add it to PATH",
            )
        if timeout_seconds is not None and timeout_seconds <= 0:
            raise ValueError("timeout_seconds must be positive")
        _check_engine_version(resolved)
        self._binary = resolved
        self._project_dir = Path(project_dir)
        self._timeout = timeout_seconds

    def plan(
        self,
        targets: Sequence[str],
        *,
        manual: bool = False,
        variables: Mapping[str, Any] | None = None,
    ) -> dict[str, Any]:
        """Execution plan as the engine prints it (`dowai plan --json`)."""
        arguments = ["plan", *_targets(targets), "--project-dir", str(self._project_dir), "--json"]
        if manual:
            arguments.append("--manual")
        arguments += _variables(variables)
        return self._payload(arguments)

    def run(
        self,
        targets: Sequence[str],
        *,
        manual: bool = False,
        profile: str | None = None,
        session_id: str | None = None,
        variables: Mapping[str, Any] | None = None,
        confirm_destructive: bool = False,
    ) -> RunResult:
        """Runs the targets (`dowai run --json`). Engine refusals come back as a failed result."""
        arguments = ["run", *_targets(targets), "--json"]
        if manual:
            arguments.append("--manual")
        if profile is not None:
            arguments += ["--profile", profile]
        if session_id is not None:
            arguments += ["--session", session_id]
        if confirm_destructive:
            arguments.append("--yes")
        arguments += _variables(variables)
        return self._result(arguments)

    def retry(
        self,
        run_id: str,
        *,
        failed_only: bool = False,
        variables: Mapping[str, Any] | None = None,
        confirm_destructive: bool = False,
    ) -> RunResult:
        """Runs a FAILED or CANCELLED run again (`dowai retry --json`)."""
        arguments = ["retry", _text(run_id, "run_id"), "--json"]
        if failed_only:
            arguments.append("--failed-only")
        if confirm_destructive:
            arguments.append("--yes")
        arguments += _variables(variables)
        return self._result(arguments)

    def runs(self, limit: int = 20) -> dict[str, Any]:
        """Most recent runs from the journal (`dowai runs list --json`)."""
        if isinstance(limit, bool) or not isinstance(limit, int) or not 1 <= limit <= 1000:
            raise ValueError("limit must be an integer from 1 to 1000")
        return self._payload(["runs", "list", "--limit", str(limit), "--json"])

    def run_detail(self, run_id: str) -> dict[str, Any]:
        """One run with its invocations (`dowai runs show --json`)."""
        return self._payload(["runs", "show", _text(run_id, "run_id"), "--json"])

    def logs(self, run_id: str, *, node: str | None = None, level: str = "INFO") -> dict[str, Any]:
        """Invocation tree of one run (`dowai logs --json`)."""
        arguments = ["logs", _text(run_id, "run_id"), "--level", _text(level, "level")]
        if node is not None:
            arguments += ["--node", node]
        arguments.append("--json")
        return self._payload(arguments)

    def _result(self, arguments: list[str]) -> RunResult:
        code, payload = self._execute(arguments)
        status = payload.get("status")
        if status not in _ENVELOPE_STATUSES and "error_code" not in payload:
            raise DowaiExecutionError("DOWAI_CLI_OUTPUT", "dowai printed an unexpected result")
        if status == "SUCCEEDED":
            return RunResult(
                status="SUCCEEDED",
                schema_version=payload.get("schema_version", 2),
                session_id=payload.get("session_id"),
                run_id=payload.get("run_id"),
                invocation_id=payload.get("invocation_id"),
                execution_mode=payload.get("execution_mode"),
                duration_ms=payload.get("duration_ms"),
                nodes=tuple(payload.get("nodes", ())),
                warnings=tuple(payload.get("warnings", ())),
            )
        return RunResult(
            status=status if status in _ENVELOPE_STATUSES else "FAILED",
            # Present only when the failed run started (ENT-015); same value as the CLI.
            run_id=payload.get("run_id"),
            error_code=payload.get("error_code") or "DOWAI_CLI_FAILED",
            message=payload.get("message") or "dowai failed",
        )

    def _payload(self, arguments: list[str]) -> dict[str, Any]:
        _, payload = self._execute(arguments)
        if "error_code" in payload:
            raise DowaiExecutionError(
                payload["error_code"], payload.get("message") or "dowai failed"
            )
        return payload

    def _execute(self, arguments: list[str]) -> tuple[int, dict[str, Any]]:
        try:
            completed = subprocess.run(
                [self._binary, *arguments],
                cwd=self._project_dir,
                capture_output=True,
                text=True,
                timeout=self._timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise DowaiExecutionError(
                "DOWAI_CLI_TIMEOUT", "dowai did not finish within the client timeout"
            ) from error
        except OSError as error:
            raise DowaiExecutionError(
                "DOWAI_CLI_NOT_FOUND", "The dowai executable could not be started"
            ) from error
        try:
            payload = json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise DowaiExecutionError(
                "DOWAI_CLI_OUTPUT", "dowai did not print a JSON result"
            ) from error
        if not isinstance(payload, dict):
            raise DowaiExecutionError("DOWAI_CLI_OUTPUT", "dowai printed a non-object result")
        return completed.returncode, payload


_VERSION_PATTERN = re.compile(r"^dowai (\d+)\.(\d+)\.(\d+)", re.MULTILINE)


def _series(version: str) -> tuple[int, int]:
    major, minor, *_ = version.split(".")
    return int(major), int(minor)


def _check_engine_version(binary: str) -> None:
    """Refuses an engine whose major.minor differs from the SDK's (pre-1.0: minor is breaking)."""
    try:
        completed = subprocess.run(
            [binary, "--version"], capture_output=True, text=True, timeout=30, check=False
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise DowaiExecutionError(
            "DOWAI_CLI_NOT_FOUND", "The dowai executable could not report its version"
        ) from error
    match = _VERSION_PATTERN.search(completed.stdout)
    if match is None:
        raise DowaiExecutionError(
            "DOWAI_VERSION_INCOMPATIBLE",
            f"Could not read the engine version; the SDK requires {_SDK_SERIES}.x",
        )
    engine_version = ".".join(match.groups())
    if _series(engine_version) != _series(_SDK_VERSION):
        raise DowaiExecutionError(
            "DOWAI_VERSION_INCOMPATIBLE",
            f"Engine {engine_version} is not compatible with dowai-sdk {_SDK_VERSION}; "
            f"use the engine {_SDK_SERIES}.x",
        )


def _text(value: str, name: str) -> str:
    if not isinstance(value, str) or not value or value.startswith("-"):
        raise ValueError(f"{name} must be a non-empty string that does not start with '-'")
    return value


def _targets(targets: Sequence[str]) -> list[str]:
    if isinstance(targets, str) or not targets:
        raise ValueError("targets must be a non-empty sequence of strings")
    return [_text(target, "target") for target in targets]


def _variables(variables: Mapping[str, Any] | None) -> list[str]:
    if variables is None:
        return []
    if not isinstance(variables, Mapping):
        raise ValueError("variables must be a mapping")
    return ["--vars", json.dumps(dict(variables), ensure_ascii=False)]
