# fi-engine: build, test and measure. `make help` lists the targets.

CARGO ?= cargo
PY := .venv/bin/python
# The Python the bindings build against (pyo3 reads it).
export PYO3_PYTHON := $(abspath $(PY))

.PHONY: help check test rust-test python python-test bench bench-full clippy fmt doc example

help:
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-12s %s\n", $$1, $$2}'

check: fmt clippy test ## What CI runs: formatting, lints, then every test

test: rust-test python-test ## Every test: Rust, then Python

rust-test: ## Rust: unit tests, the spec's invariants as property tests, generality scenarios
	$(CARGO) test

$(PY):
	uv venv .venv --python 3.13
	uv pip install --python $(PY) --exclude-newer 2026-11-01T00:00:00Z maturin polars numpy pandas pyarrow pytest

python: $(PY) ## Build the Python extension into .venv (release)
	.venv/bin/maturin develop --release

python-test: python ## Python: data, models, runs, examples, and the Python speed tests
	$(PY) -m pytest -q

bench: ## Speed tests against the spec's budgets, at a quarter of full scale
	$(CARGO) run --release -p fi-bench -- --scale 0.25 --check

bench-full: ## The same at full scale: 9,000 bonds × 2,520 days (about 3.5 GB of memory)
	$(CARGO) run --release -p fi-bench -- --scale 1 --check

clippy: $(PY) ## Lints on every crate and test, warnings as errors
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt: ## Check formatting (rustfmt.toml)
	$(CARGO) fmt --all --check

doc: ## API documentation for the workspace's crates
	$(CARGO) doc --no-deps --workspace

example: python ## Run the Python quickstart
	$(PY) examples/quickstart.py
