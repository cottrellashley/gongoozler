#!/bin/sh
# One fuzz target, for `make fuzz`: tools/fuzz.sh PACKAGE TARGET FUZZTIME.
#
# Go's fuzzing coordinator can report its own -fuzztime deadline as a failure: when the deadline
# fires it passes the context's error to stop(), which suppresses it only if it equals its child
# context's, and the child is cancelled just after the parent's done channel closes (kb/go.md).
# Such a run prints "context deadline exceeded" and writes no failing input. That case, and only
# that case, is run once more; a real failure writes its input and fails as it is.
set -u
pkg=$1 target=$2 time=$3
for attempt in 1 2; do
    out=$(go test "$pkg" -run '^$' -fuzz "^$target\$" -fuzztime "$time" -fuzzminimizetime 5s 2>&1)
    code=$?
    printf '%s\n' "$out"
    [ "$code" -eq 0 ] && exit 0
    if printf '%s' "$out" | grep -q "Failing input written to" || ! printf '%s' "$out" | grep -q "context deadline exceeded"; then
        exit "$code"
    fi
    echo "fuzz: $target: the coordinator reported its own deadline, with no failing input (attempt $attempt)"
done
exit 1
