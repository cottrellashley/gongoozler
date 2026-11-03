"""Run parameters: their defaults live in the file; the CLI or a sweep sets their values.

The values arrive as a JSON object in GONGOOZLER_PARAMS. A name the file does not declare is an
error, so a sweep cannot set a parameter that silently does nothing.
"""

from __future__ import annotations

import json
import os
from types import SimpleNamespace
from typing import Any

from . import _events as ev

ENV_PARAMS = "GONGOOZLER_PARAMS"
_TRUE = {"true", "1", "yes", "on"}
_FALSE = {"false", "0", "no", "off"}
_current: dict[str, Any] = {}


def current() -> dict[str, Any]:
    """The values the last call to `params` settled on (for a run's manifest)."""
    return dict(_current)


def params(**defaults: Any) -> SimpleNamespace:
    """The run's parameters: `defaults`, overridden by GONGOOZLER_PARAMS. A value takes the type of
    its default. The final values are sent as a `params` event."""
    raw = os.environ.get(ENV_PARAMS)
    given = json.loads(raw) if raw else {}
    if not isinstance(given, dict):
        raise ValueError(f"{ENV_PARAMS} must hold a JSON object, not {raw!r}")
    unknown = sorted(set(given) - set(defaults))
    if unknown:
        raise KeyError(f"unknown parameter(s) {', '.join(unknown)}; this file declares {', '.join(sorted(defaults)) or 'none'}")
    values = {k: _coerce(k, given[k], d) if k in given else d for k, d in defaults.items()}
    _current.clear()
    _current.update(values)
    ev.emit("params", values=values)
    return SimpleNamespace(**values)


def _coerce(name: str, value: Any, default: Any) -> Any:
    if default is None:
        return value
    if isinstance(default, bool):
        if isinstance(value, bool):
            return value
        if isinstance(value, str) and value.lower() in _TRUE | _FALSE:
            return value.lower() in _TRUE
    elif isinstance(default, int):
        if isinstance(value, int) and not isinstance(value, bool):
            return value
        if isinstance(value, float) and value.is_integer():
            return int(value)
        if isinstance(value, str):
            try:
                return int(value)
            except ValueError:
                pass
    elif isinstance(default, float):
        if isinstance(value, int | float) and not isinstance(value, bool):
            return float(value)
        if isinstance(value, str):
            try:
                return float(value)
            except ValueError:
                pass
    elif isinstance(default, str):
        return str(value)
    elif isinstance(value, type(default)):
        return value
    raise TypeError(f"parameter {name}: {value!r} is not a {type(default).__name__}")
