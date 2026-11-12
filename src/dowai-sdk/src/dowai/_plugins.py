"""Plugin call facade (ENT-106): `env.plugins.run("ns.m")` and `env.plugins.ns.m()`.

Both forms go through one resolution path, `resolve`, which asks the engine's single resolver
(`dowai plugins resolve`). Resolution never imports or executes plugin code. Plugin execution is a
separate task: a call that passes keyword arguments is resolved first and then refused with
`DOWAI_PLUGIN_EXECUTION_UNAVAILABLE`, so arguments are never dropped silently.
"""

from __future__ import annotations

import re
from collections.abc import Callable
from typing import Any

from ._errors import DowaiExecutionError

_TARGET_PATTERN = re.compile(r"^[A-Za-z_][A-Za-z0-9_-]*\.[A-Za-z_][A-Za-z0-9_-]*$")

Resolver = Callable[[str], dict[str, Any]]


class PluginCalls:
    """Facade returned by `Env.plugins`. Attribute access builds the namespace path."""

    def __init__(self, resolve: Resolver) -> None:
        self._resolve = resolve

    def run(self, target: str, **kwargs: Any) -> dict[str, Any]:
        """Resolves `namespace.method`; returns the engine's resolution."""
        if not isinstance(target, str) or not _TARGET_PATTERN.match(target):
            raise ValueError("target must have the form namespace.method")
        resolution = self._resolve(target)
        if kwargs:
            raise DowaiExecutionError(
                "DOWAI_PLUGIN_EXECUTION_UNAVAILABLE",
                f"Plugin {target} was resolved but not executed: plugin execution is not "
                "available yet.",
            )
        return resolution

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
