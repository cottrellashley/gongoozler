# What we rely on from marimo

Docs read 3 Nov 2026:
- https://docs.marimo.io/guides/package_management/sandboxes/
- https://docs.marimo.io/guides/scripts/

Not yet exercised here.

| Fact | Source |
| --- | --- |
| A marimo notebook is a `.py` file, and can carry its package requirements in the same file as PEP 723 inline metadata | Read: sandboxes |
| `uvx marimo edit --sandbox notebook.py` prepares an isolated environment from those requirements, with uv | Read: sandboxes |
| `uv run notebook.py` runs a notebook as a script, installing its inline requirements; plain `python notebook.py` uses the current environment | Read: scripts |
| Arguments reach a notebook run as a script through `sys.argv` | Read: scripts |
| `marimo export html notebook.py -o notebook.html` runs the notebook and exports its outputs; arguments follow `--` | Read: scripts |
| `uv lock --script notebook.py` locks a notebook as it locks a script | Read: sandboxes |

So a marimo notebook is a run file like any other. In a box, `marimo export html` also makes the
executed notebook the run's report: a later slice.
