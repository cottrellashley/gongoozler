# /// script
# requires-python = ">=3.13"
# dependencies = ["gongoozler", "nautilus_trader==2.0.0rc8"]
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Run once as the box image is built: it puts gongoozler and Nautilus in uv's cache, so a run
that needs them starts without downloading them."""

import gongoozler
import nautilus_trader

print("warm:", "gongoozler", gongoozler.__version__, "nautilus_trader", nautilus_trader.__version__)
