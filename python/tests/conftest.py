from __future__ import annotations

import os
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest

from gongoozler import _events, _ring

from .ringread import Reader

SRC = Path(__file__).resolve().parents[1] / "src"


class Captured:
    """The events this process wrote into a ring during a test, read back the spec's way."""

    def __init__(self, directory: Path, transport: _events.RingTransport) -> None:
        self.dir = directory
        self.transport = transport
        self.reader = Reader(str(transport.ring.path))
        self._events: list[dict] = []

    def events(self) -> list[dict]:
        self._events.extend(self.reader.drain())
        return self._events

    def kinds(self) -> list[str]:
        return [e["kind"] for e in self.events()]

    def of(self, kind: str) -> list[dict]:
        return [e for e in self.events() if e["kind"] == kind]

    def counters(self) -> dict[str, int]:
        return self.reader.counters()


class Recording(_events._Counting):
    """Keeps every event and every progress and metric update, in order: the whole history a
    ring's slots coalesce away, for tests of what a driver reports."""

    def __init__(self) -> None:
        super().__init__()
        self.events: list[dict] = []
        self.units: dict[str, str] = {}

    def ordered(self, em: _events.Emitter, ev: dict, payload: bytes) -> None:
        if ev["kind"] == "progress.unit":  # merged into progress, as the supervisor's reader does
            self.units[ev["key"]] = ev["unit"]
            return
        self.events.append(ev)

    def latest(self, em: _events.Emitter, kind: str, key: str, fields: dict, t: float) -> None:
        ev = {"kind": kind, "t": t, **fields}
        if kind == "progress" and key in self.units:
            ev["unit"] = self.units[key]
        self.events.append(ev)

    def of(self, kind: str) -> list[dict]:
        return [e for e in self.events if e["kind"] == kind]


@pytest.fixture(autouse=True)
def artifacts(tmp_path_factory: pytest.TempPathFactory, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Every test writes its artifacts to a directory of its own, never into the checkout."""
    d = tmp_path_factory.mktemp("artifacts")
    monkeypatch.setenv("GONGOOZLER_ARTIFACTS", str(d))
    return d


@pytest.fixture
def recorded() -> Iterator[Recording]:
    """Routes this process's events, every update included, to a list for the test."""
    r = Recording()
    with _events._capture(r):
        yield r


@pytest.fixture
def captured(tmp_path: Path) -> Iterator[Captured]:
    """Routes this process's events into a ring in a temporary directory for the test."""
    transport = _events.RingTransport(_ring.Ring(tmp_path))
    with _events._capture(transport):
        c = Captured(tmp_path, transport)
        yield c


def run_script(code: str, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    """Runs `code` in a fresh interpreter that imports this checkout's gongoozler."""
    full = {**os.environ, "PYTHONPATH": str(SRC), **env}
    full.pop("GONGOOZLER_EVENTS", None)
    return subprocess.run([sys.executable, "-c", code], check=False, env={**full, **env}, capture_output=True, text=True, timeout=60)
