# What we rely on from uv and PEP 723

Pinned: uv 0.12.10 (the build backend is `uv_build` 0.12). Docs read:
https://docs.astral.sh/uv/guides/scripts/.

| Fact | Source |
| --- | --- |
| PEP 723 inline metadata: a `# /// script` block with `requires-python` and `dependencies`; "The `dependencies` field must be provided even if empty" | Read |
| `uv run file.py` and `uv run --script file.py` install the declared dependencies and run the file | Read; observed |
| `uv lock --script file.py` writes `file.py.lock` beside the file. Scripts are locked only when asked; once locked, `uv run --script` reuses the lock | Read; observed (`tools/spikes/*.lock`) |
| `[tool.uv] exclude-newer = "<RFC 3339>"` limits resolution to releases before a date | Read |
| `[[tool.uv.index]]` with a `url` in the header points at another index (`uv add --index … --script` writes it) | Read |
| `[tool.uv.sources]` in a header takes a path source: `gongoozler = { path = "../python" }`, relative to the file | Observed (uv 0.12.10) |
| **Use `editable = true` for a source being worked on.** A plain path source is built once and cached: after editing the source, cached script environments still held the old build | Observed, 3 Nov: every cached environment lacked a constant added after its build |
| A script's environment is cached under the cache's `environments-v2/` and reused while its requirements are unchanged | Observed |
| PEP 723 lets any tool use a `[tool]` table in the header: gongoozler's is `[tool.gongoozler]` (memory, cpus, timeout, placement, sweep; spec/run-file.md) | PEP 723 |
| `uv lock --script` works offline for a file with no dependencies, and writes a lock all the same | Observed |
| `UV_FIND_LINKS=DIR` resolves a name from wheels in a directory: a box resolves `gongoozler` from the wheel in its image (`uv build --wheel`) | Read |
| **Under `PYTHONUNBUFFERED=1`, each `print` is two writes** (the text, then the newline). Reconfigured to line buffering (`sys.stdout.reconfigure(line_buffering=True, write_through=False)`), it is one, still shown as printed: a chatty run went from 1.53× to 0.94× its bare time under the CLI | Observed (`kb/measurements.md`) |
| uv resolves for every Python a project allows: a dependency needing 3.13+ must carry `; python_version >= '3.13'` when the project allows 3.12 | Observed |
| A maturin project as a path source (`fi-engine = { path = "../engine", editable = true }`) builds through maturin's PEP 517 hooks in release, reusing the engine's own `target/`; its `[tool.uv] cache-keys` (here globs over `crates/**/*.rs` and the manifests) decide when uv rebuilds it | Observed: `uv sync` in 18 s on a warm `target/`; records at release speed | `python/pyproject.toml`, `engine/pyproject.toml` |
| An editable maturin install puts the extension in the source tree (`engine/python/fi_engine/_native.abi3.so`), the same file `maturin develop` writes: the engine's venv and gongoozler's share it | Observed | `make check` |
| `uv build --wheel engine` builds the engine's wheel (`cp311-abi3-linux_x86_64`), and `UV_FIND_LINKS` resolves `gongoozler[fi]` from it as from gongoozler's | Observed: `examples/fi_in_a_box.py` run against the two wheels alone | The box image |

Measured (`kb/measurements.md`): locking a Nautilus file took 0.1–0.3 s. A cold `uv run`, with an
empty cache, took 8.8 s, mostly downloading Nautilus's 74 MB wheel. Warm, uv added about 50 ms.
