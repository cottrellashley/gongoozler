# What we rely on from Go

Pinned: Go 1.25 (`go.mod`; CI's `setup-go`, here 1.25.14). "Read" means taken from Go's source or
docs; "observed" means seen in this repo's runs.

| Fact | Source | Used by |
| --- | --- | --- |
| A fuzz target run with `-fuzztime DURATION` can fail with only "context deadline exceeded" and no failing input: at the deadline, `CoordinateFuzzing` calls `stop(ctx.Err())`, which drops the error only if it equals `fuzzCtx.Err()`, and `fuzzCtx` (a child of the timed context) is cancelled just after the parent's done channel closes | Read: `src/internal/fuzz/fuzz.go`, `CoordinateFuzzing` and `stop`; `src/context/context.go`, `cancelCtx.cancel` closes `done` before cancelling children. Observed: CI run 41760238170, `FuzzRing` on a loaded runner | `tools/fuzz.sh` reruns that case once |
| `-fuzzminimizetime` bounds minimising a large seed, which otherwise looks like a stall | Observed (an earlier `make fuzz`) | `tools/fuzz.sh` |
| Test results are cached by their Go inputs, files they open and the environment they read: an engine change behind a subprocess does not invalidate them | Read: `go help test`, "test caching". Observed: the CLI's engine test reported cached | Run `go test -count=1` after an engine change |
