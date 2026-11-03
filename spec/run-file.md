# The run file

This page is normative. `internal/header` reads a file's header, `cmd/gongoozler` acts on it, and
`python/src/gongoozler/params.py` gives the file its parameters. Tests:
`internal/header/header_test.go` (with `FuzzParse`), `internal/sweep`, and the CLI's.

A run is one Python file, a script or a marimo notebook, run by `uv run --script`. Its header
says what it needs (PEP 723); `[tool.gongoozler]` in it says how big it is. Everything a person
writes is Python: no YAML, no job specs.

```python
# /// script
# requires-python = ">=3.13"
# dependencies = ["gongoozler", "nautilus_trader==2.0.0rc8", "your-data-library"]
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# [tool.gongoozler]
# memory = "48GiB"                            # a limit, and what placement weighs
# cpus = 8
# timeout = "3h"
# placement = "auto"                          # auto, local or remote
# sweep = { fast = [10, 20, 30], slow = [50, 100] }
# ///
import gongoozler as gz

p = gz.params(fast=10, slow=50)               # the defaults; the CLI or a sweep sets values
```

## `[tool.gongoozler]`

| Setting | Meaning | Default |
| --- | --- | --- |
| `memory` | The run's memory limit: bytes, or `"48GiB"`, `"512 MiB"`, `"2G"`. Enforced by its cgroup (here) or container (in a box); weighed by placement | none |
| `cpus` | Its CPU limit (`CPUQuota`, or the container's), and what a local sweep divides the cores by | none |
| `timeout` | Its time limit: seconds, or `"3h"`, `"90m"` | none |
| `placement` | `auto`, `local` or `remote` (below) | `auto` |
| `sweep` | Parameter names and lists of values (below) | none |

Any other key is an error that names the keys there are, and so is a value of the wrong kind.
The header is read with a strict subset of TOML (`internal/header/toml.go`), fuzzed.
`--memory`, `--cpus` and `--timeout` override the header.

## Parameters

`gz.params(**defaults)` returns the run's parameters: its defaults, overridden by
`GONGOOZLER_PARAMS` (a JSON object the supervisor sets from `--param NAME=VALUE`, or a sweep's
point). A value takes its default's type (`"12"` for an int default is 12). A name the file does
not declare is an error, so a sweep cannot set a parameter that silently does nothing. The
settled values are sent as a `params` event, and kept in the manifest.

## Where it runs

`gongoozler run` decides, in this order:
1. `--local` or `--remote`;
2. the file's `placement`, if `local` or `remote` (the file knows best: it may read local files);
3. `$GONGOOZLER_PLACEMENT`, if `local` or `remote` (a cloud harness sets `remote`);
4. **`auto`:** remote when the file declares more `memory` than this machine has available and a
   server is set (`$GONGOOZLER_SERVER`); here otherwise. The CLI says why it placed a run where
   it did.

Remote runs are watched exactly as local ones (spec/server.md). Publishing is for capacity, not
for sharing.

## The lock

On its first run, a file with a script block is locked: the CLI runs `uv lock --script FILE`,
which writes `FILE.lock` beside it, and says so. Every later run, here or in a box, resolves the
same packages; the run's meta records the lock's hash, and the lock is kept with the run.
`--no-lock` skips this. A file with no script block has nothing to lock. If locking fails
(offline, say), the run goes on and says it resolves afresh.

## Sweeps

`gongoozler sweep FILE` runs the file once for every point of a grid:
- the grid is the header's `sweep`, extended and overridden by `--param NAME=V1,V2,...` (or a JSON
  list, `--param 'NAME=[1, "a,b"]'`); `--set NAME=VALUE` holds a parameter at one value;
- points are every combination, in a stable order (names sorted, the last varying fastest);
- **here,** `--jobs` runs at once (default: the cores over the file's `cpus`, and no more than the
  available memory over its `memory`), each under the supervisor, as `run` would;
- **on the server** (placement as above), each point is a run of its own, in a box of its own;
- `--rank STAT` orders the runs by a statistic in their `stats.json`, by a dotted path
  (`"pnls.USD.PnL (total)"`), highest first, or lowest first with a leading `-`.

The sweep is recorded in `STATE/sweeps/ID.json` (its grid, ranking and runs), and each run's
meta names its sweep. It exits 0 when every run was `ok`, 1 otherwise, 130 when interrupted (an
interrupt cancels the runs going and starts no more).
