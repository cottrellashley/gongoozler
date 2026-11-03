"""What a run keeps: results and their manifest, as deterministic artifacts (spec/results.md).

Every file is canonical JSON (keys sorted, no spaces, non-finite numbers as null), one object per
line for tables. The same run on the same inputs writes byte-identical files, so a rerun proves
itself by its hashes: `gongoozler diff` and `gongoozler rerun` compare them. Fields that differ
between identical runs (random event IDs, wall-clock times) are left out, and the environment
(host, platform) is kept apart from the inputs, in `manifest.json`'s `environment`.
"""

from __future__ import annotations

import importlib.metadata
import json
import platform
import sys
from collections.abc import Iterable, Mapping
from pathlib import Path
from typing import Any

from . import _events as ev


def canonical(obj: Any) -> str:
    """One value as canonical JSON."""
    return json.dumps(ev.clean(obj), sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False)


def write_json(name: str, obj: Any) -> Path:
    """Writes one object as an artifact."""
    path = ev.artifacts_dir() / name
    path.write_text(canonical(obj) + "\n", encoding="utf-8")
    return ev.artifact(path)


def write_jsonl(name: str, rows: Iterable[Mapping[str, Any]]) -> Path:
    """Writes a table, one object per line, as an artifact."""
    path = ev.artifacts_dir() / name
    with path.open("w", encoding="utf-8") as f:
        for row in rows:
            f.write(canonical(row) + "\n")
    return ev.artifact(path)


def versions(*packages: str) -> dict[str, str | None]:
    """The installed versions of `packages` (None for one not installed)."""
    out: dict[str, str | None] = {}
    for p in packages:
        try:
            out[p] = importlib.metadata.version(p)
        except importlib.metadata.PackageNotFoundError:
            out[p] = None
    return out


def manifest(**inputs: Any) -> dict[str, Any]:
    """A run's manifest: what produced its results. `inputs` (feeds, engine, windows) join the
    parameters and package versions; `environment` says where it ran, apart from the inputs."""
    from . import __version__
    from .params import current

    return {
        "gongoozler": __version__,
        "params": current(),
        "packages": versions("gongoozler", "nautilus_trader"),
        "python": sys.version.split()[0],
        "inputs": inputs,
        "environment": {"platform": platform.platform(), "machine": platform.machine(), "node": platform.node()},
    }
