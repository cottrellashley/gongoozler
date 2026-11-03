# gongoozler. `make check` is what CI runs: formatting, lints, type checks, and every test, Go's
# under the race detector. `make bench` holds the budgets (tools/bench/bench.py). `make fuzz` fuzzes
# what reads bytes a run wrote.

GO_TEST_FLAGS ?= -race
FUZZTIME      ?= 30s
UV            ?= uv
RUFF          ?= ruff@0.14.2
MYPY          ?= mypy@1.18.2
GOLANGCI      ?= go run github.com/golangci/golangci-lint/v2/cmd/golangci-lint@v2.5.0
IMAGE         ?= gongoozler

.PHONY: check fmt vet lint test-go test-py build fuzz bench docker e2e-docker clean

check: fmt vet lint test-go test-py

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
	go test ./internal/events -run '^$$' -fuzz '^FuzzDecode$$' -fuzztime $(FUZZTIME) -fuzzminimizetime 5s
	go test ./internal/ring -run '^$$' -fuzz '^FuzzRing$$' -fuzztime $(FUZZTIME) -fuzzminimizetime 5s
	go test ./internal/ring -run '^$$' -fuzz '^FuzzOpen$$' -fuzztime $(FUZZTIME) -fuzzminimizetime 5s
	go test ./internal/header -run '^$$' -fuzz '^FuzzParse$$' -fuzztime $(FUZZTIME) -fuzzminimizetime 5s

bench: build
	cd python && $(UV) run --quiet python ../tools/bench/bench.py

# A box image: the CLI, uv and Python, with a warm package cache (Dockerfile).
docker:
	docker build -t $(IMAGE) .

# The server, a worker running jobs in containers of $(IMAGE), and a run submitted to them.
e2e-docker: docker
	./tools/e2e-docker.sh $(IMAGE)

clean:
	rm -rf bin python/.pytest_cache python/.mypy_cache python/.ruff_cache
	find . -name __pycache__ -type d -prune -exec rm -rf {} +
