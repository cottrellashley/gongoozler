# Working in gongoozler

- **`make check` must pass before any change is called done:** gofmt and ruff format, `go vet`,
  golangci-lint, ruff, mypy (strict), `go test -race ./...`, and the Python tests
  (`cd python && uv run pytest`, which brings in Nautilus 2.0.0rc8). A change to the hot path also
  passes `make bench`; a change to what reads a run's bytes (the decoder, the ring, the header)
  also passes `make fuzz`.
- **The specs are normative.** A change to the events, the ring, the supervisor's guarantees, the
  feed contract, results, the run file or the server's API changes its page in `spec/`, both sides
  (Python and Go), and their tests, together.
- **Never block the run.** Nothing in the emitter or the drivers may wait on a viewer, a socket, a
  disk or a thread in the run's own process. Events go into the ring, and the supervisor reads them
  from outside.
- **The supervisor is the source of truth.** What is known about how a run ended comes from
  outside it: its cgroup, its exit, its process tree. Never trust a run to report its own end.
- **Results are deterministic.** The same run on the same inputs writes the same bytes: no wall
  times, random IDs or host names in results (the environment goes in the manifest's
  `environment`).
- **Never read anyone's data.** Data libraries define their reads and feed streams; gongoozler
  checks the streams and drives the engine.
- **No YAML for users.** A run is a Python file; its sizing is its PEP 723 header. (CI's workflow is
  YAML because GitHub reads nothing else.)
- **Claims need evidence.** A performance claim needs a script in `tools/` and a line in
  `kb/measurements.md`. A fact about a dependency needs a row in `kb/`, marked read or observed.
- **Dev path sources in PEP 723 headers are `editable = true`.** A plain path source is built once
  and cached, and goes stale (`kb/uv.md`). A file meant for boxes names `gongoozler` with no path
  source: a box resolves it from its image's wheel.
- **Shell commands:** use absolute paths, since parallel shell calls share one working directory.
  Never `pkill -f` or `pgrep -f` a pattern that appears in your own command line.
