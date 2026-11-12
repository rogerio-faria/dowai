"""Plugin call facade (ENT-106, ENT-107): `env.plugins.run("ns.m", **kwargs)` and
`env.plugins.ns.m(**kwargs)`.

Both forms go through one path, `run`, which asks the engine to execute the plugin
(`dowai plugins run`). The engine resolves the target first, runs the entrypoint in an isolated
subprocess and returns its result envelope unchanged, so the result equals the CLI's. Keyword
arguments become the JSON input of the entrypoint; `idempotency_key` is reserved for the engine
and is not passed to the plugin. `resolve` only resolves and never executes plugin code.
"""

from __future__ import annotations

import json
import re
from collections.abc import Callable
from typing import Any

_TARGET_PATTERN = re.compile(r"^[A-Za-z_][A-Za-z0-9_-]*\.[A-Za-z_][A-Za-z0-9_-]*$")

Resolver = Callable[[str], dict[str, Any]]
Runner = Callable[[str, dict[str, Any], str | None], dict[str, Any]]


def _check_target(target: str) -> None:
    if not isinstance(target, str) or not _TARGET_PATTERN.match(target):
        raise ValueError("target must have the form namespace.method")


class PluginCalls:
    """Facade returned by `Env.plugins`. Attribute access builds the namespace path."""

    def __init__(self, resolve: Resolver, run: Runner) -> None:
        self._resolve = resolve
        self._run = run

    def resolve(self, target: str) -> dict[str, Any]:
        """Resolves `namespace.method`; returns the engine's resolution without executing."""
        _check_target(target)
        return self._resolve(target)

    def run(
        self, target: str, /, *, idempotency_key: str | None = None, **kwargs: Any
    ) -> dict[str, Any]:
        """Executes `namespace.method` with `kwargs` as JSON input; returns the engine's result."""
        _check_target(target)
        try:
            json.dumps(kwargs)
        except (TypeError, ValueError) as error:
            raise ValueError("plugin arguments must be JSON-serializable") from error
        return self._run(target, dict(kwargs), idempotency_key)

    def __getattr__(self, namespace: str) -> "_PluginNamespace":
        if namespace.startswith("_"):
            raise AttributeError(namespace)
        return _PluginNamespace(self, namespace)


class _PluginNamespace:
    def __init__(self, calls: PluginCalls, namespace: str) -> None:
        self._calls = calls
        self._namespace = namespace

    def __getattr__(self, method: str) -> Callable[..., dict[str, Any]]:
        if method.startswith("_"):
            raise AttributeError(method)
        target = f"{self._namespace}.{method}"

        def call(**kwargs: Any) -> dict[str, Any]:
            return self._calls.run(target, **kwargs)

        call.__name__ = method
        return call
