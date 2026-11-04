# gongoozler. `make check` is what CI runs: formatting, lints, type checks, and every test, Go's
# under the race detector, and the engine's own (engine/Makefile). `make bench` holds the budgets
# (tools/bench/bench.py, and the engine's at full scale; `TOLERANCE` passes to the engine's, see
# engine/Makefile). `make fuzz` fuzzes what reads bytes a run wrote.

GO_TEST_FLAGS ?= -race
FUZZTIME      ?= 30s
UV            ?= uv
RUFF          ?= ruff@0.14.2
MYPY          ?= mypy@1.18.2
GOLANGCI      ?= go run github.com/golangci/golangci-lint/v2/cmd/golangci-lint@v2.5.0
IMAGE         ?= gongoozler

.PHONY: check fmt vet lint py-env engine test-go test-py build fuzz bench bench-full docker e2e-docker clean

check: fmt vet lint engine py-env test-go test-py

# The engine: rustfmt, clippy, its Rust tests and its Python tests (engine/Makefile).
engine:
	$(MAKE) -C engine check

# The Python environment the tests run in, with the engine built into it (the CLI's engine tests
# use its interpreter).
py-env:
	cd python && $(UV) sync --quiet

fmt:
	@test -z "$$(gofmt -l .)" || { gofmt -l .; echo "gofmt: the files above need formatting"; exit 1; }
	cd python && $(UV) tool run $(RUFF) format --check src tests ../tools

vet:
	go vet ./...

lint:
	$(GOLANGCI) run ./...
	cd python && $(UV) tool run $(RUFF) check src tests ../tools
	cd python && $(UV) tool run --with numpy $(MYPY) src

test-go:
	go test $(GO_TEST_FLAGS) ./...

test-py:
	cd python && $(UV) run --quiet pytest -q

build:
	go build -o bin/gongoozler ./cmd/gongoozler

fuzz:
	./tools/fuzz.sh ./internal/events FuzzDecode $(FUZZTIME)
	./tools/fuzz.sh ./internal/ring FuzzRing $(FUZZTIME)
	./tools/fuzz.sh ./internal/ring FuzzOpen $(FUZZTIME)
	./tools/fuzz.sh ./internal/header FuzzParse $(FUZZTIME)

bench: build py-env
	cd python && $(UV) run --quiet python ../tools/bench/bench.py
	$(MAKE) -C engine bench

# The engine's budgets at the spec's own scale, 9,000 bonds × 2,520 days: about 3.5 GB (CI runs it).
bench-full:
	$(MAKE) -C engine bench-full

# A box image: the CLI, uv and Python, with a warm package cache (Dockerfile).
docker:
	docker build -t $(IMAGE) .

# The server, a worker running jobs in containers of $(IMAGE), and a run submitted to them.
e2e-docker: docker
	./tools/e2e-docker.sh $(IMAGE)

clean:
	rm -rf bin python/.pytest_cache python/.mypy_cache python/.ruff_cache
	find . -name __pycache__ -type d -prune -exec rm -rf {} +
