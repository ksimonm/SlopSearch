# SlopenSearch developer entry point. Only Docker is required; `make help` lists targets.
#
#   make go-test                       # build + start the Go server, run the whole suite
#   make rust-test ARGS="-m aggs"      # Rust server, one area (pytest marker)
#   make go-test ARGS=tests/test_41_aggs_bucket.py
#   make go-score                      # pass rate per test file

ARGS ?=
# parallel pytest workers (one test file each); more than 4 overloads the reference
# OpenSearch cluster manager. JOBS=0 runs sequentially.
JOBS ?= 4
# traceback style. pytest spends ~0.3 s building a detailed report per failed test, which
# dominates runs with thousands of failures; use TB=long when debugging a few tests.
TB ?= line
COMPOSE := docker compose
# run as the host user so reports and recordings in the working tree are not owned by root
RUN_TESTS := $(COMPOSE) run --rm --build --user "$(shell id -u):$(shell id -g)" tests
PYTEST := pytest -n $(JOBS) --dist loadfile --tb=$(TB)

.DEFAULT_GOAL := help
.PHONY: help go-up rust-up ref-up go-test rust-test ref-test go-score rust-score ref-score \
        ref-show ref-record \
        go-dev rust-dev test go-logs rust-logs down

help: ## list targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-12s %s\n", $$1, $$2}'

go-up: ## build and start the Go server on :9291
	$(COMPOSE) up -d --build --wait go

rust-up: ## build and start the Rust server on :9292
	$(COMPOSE) up -d --build --wait rust

ref-up: ## start reference OpenSearch 3.9.0 on :9290
	$(COMPOSE) up -d --wait opensearch

go-test: go-up ## run tests against the Go server (ARGS="-m aggs" / a file / -k expr)
	$(RUN_TESTS) env OS_HOST=go $(PYTEST) $(ARGS)

rust-test: rust-up ## run tests against the Rust server
	$(RUN_TESTS) env OS_HOST=rust $(PYTEST) $(ARGS)

ref-test: ref-up ## run tests against reference OpenSearch (to check a new test)
	$(RUN_TESTS) env OS_HOST=opensearch $(PYTEST) $(ARGS)

go-score: go-up ## pass rate per test file for the Go server
	$(RUN_TESTS) env OS_HOST=go python scripts/score.py --jobs $(JOBS) --label go $(ARGS)

rust-score: rust-up ## pass rate per test file for the Rust server
	$(RUN_TESTS) env OS_HOST=rust python scripts/score.py --jobs $(JOBS) --label rust $(ARGS)

ref-show: ## what the reference OpenSearch answered in a test, no OpenSearch needed: make ref-show T=<name part>
	@$(RUN_TESTS) python scripts/ref.py show "$(T)"

ref-record: ref-up ## re-record tests/reference after changing tests (ARGS=test files, default all)
	$(RUN_TESTS) env OS_HOST=opensearch python scripts/ref.py record --jobs $(JOBS) $(ARGS)

ref-score: ref-up ## pass rate per test file for reference OpenSearch (should be 100%)
	$(RUN_TESTS) env OS_HOST=opensearch python scripts/score.py --jobs $(JOBS) --label opensearch $(ARGS)

go-dev: ## run the Go server natively on :9291 (needs Go)
	cd servers/go && PORT=9291 go run .

rust-dev: ## run the Rust server natively on :9292 (needs Rust)
	cd servers/rust && PORT=9292 cargo run --release

test: ## run tests against any server: make test PORT=9291 [HOST=host.docker.internal] ARGS=...
	$(RUN_TESTS) env OS_HOST=$(or $(HOST),host.docker.internal) OS_PORT=$(PORT) $(PYTEST) $(ARGS)

go-logs: ## follow Go server logs
	$(COMPOSE) logs -f go

rust-logs: ## follow Rust server logs
	$(COMPOSE) logs -f rust

down: ## stop everything
	$(COMPOSE) --profile tools down
