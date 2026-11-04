# /// script
# requires-python = ">=3.13"
# dependencies = ["gongoozler[fi]", "nautilus_trader==2.0.0rc8", "numpy", "polars"]
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Run once as the box image is built: it puts gongoozler, its engine and Nautilus in uv's cache, so
a run that needs them starts without downloading them."""

import fi_engine
import gongoozler
import nautilus_trader

print("warm:", "gongoozler", gongoozler.__version__, "fi_engine", fi_engine.__version__, "nautilus_trader", nautilus_trader.__version__)
