"""gongoozler: run a backtest file here or in a box, and watch it run.

What a run file uses:
- progress: `stage`, `progress`, `metric`, `log`, `artifact`, `emit`;
- parameters: `params`;
- the feed contract: `Feed`, `Batch`, `feed`, `from_items`, `windows`, `FeedError`;
- results: `artifacts_dir`, and `gongoozler.results` (`write_json`, `write_jsonl`, `manifest`);
- `gongoozler.nautilus` (it needs nautilus_trader): `run` streams feeds of Python objects, `run_node`
  streams catalog feeds in Rust; both keep their results. Also `tearsheet`;
- `gongoozler.catalog` (it needs nautilus_trader): `write` and `cached` put a feed in a Parquet
  catalog once, for every run after.

See spec/protocol.md for the events, spec/feeds.md for the feed contract and spec/results.md for
what a run keeps.
"""

__version__ = "0.1.0"

from ._events import PROTOCOL, artifact, artifacts_dir, emit, log, metric, progress, stage, throttled
from .feeds import Batch, Feed, FeedError, Window, feed, from_items, to_ns, windows
from .params import params

__all__ = [
    "PROTOCOL",
    "Batch",
    "Feed",
    "FeedError",
    "Window",
    "artifact",
    "artifacts_dir",
    "emit",
    "feed",
    "from_items",
    "log",
    "metric",
    "params",
    "progress",
    "stage",
    "throttled",
    "to_ns",
    "windows",
]
