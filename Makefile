# Every command a developer or CI needs. `make help` lists them.
.DEFAULT_GOAL := help
SHELL := /bin/sh

CARGO ?= cargo
UV_RUN := uv run --project services --locked
PIP_AUDIT_VERSION := 2.10.1

.PHONY: help setup env up down db-url check rust-check py-check private-check test-all audit differential-full bench crash gateway-bench dependency-age fuzz-campaign resolution-study

SEED ?= 1
RUN ?=
METRICS ?= var/marketmaker/metrics.jsonl

.PHONY: research research-check research-report bot-monitor

research: ## Run paired stress experiments and create an offline dashboard (no gateway required)
	$(UV_RUN) python -m brier_services.research run --config research/stress.example.json

research-check: ## Verify and replay a saved experiment: make research-check RUN=var/research/<id>
	@test -n "$(RUN)" || { echo 'Set RUN to an experiment directory'; exit 1; }
	$(UV_RUN) python -m brier_services.research verify "$(RUN)"
	$(UV_RUN) python -m brier_services.research replay "$(RUN)"

research-report: ## Rebuild an offline dashboard: make research-report RUN=var/research/<id>
	@test -n "$(RUN)" || { echo 'Set RUN to an experiment directory'; exit 1; }
	$(UV_RUN) python -m brier_services.research report "$(RUN)" --out "$(RUN).html"

bot-monitor: ## Read-only bot decision dashboard at localhost:3010; override METRICS if needed
	$(UV_RUN) python -m brier_services.research monitor --metrics "$(METRICS)"

help: ## List every command
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*## "} {printf "  %-14s %s\n", $$1, $$2}'

setup: env ## Locked Python dependencies, git hooks, and a local .env (Rust installs from rust-toolchain.toml)
	uv sync --project services --locked
	pre-commit install

env: ## Create .env with a generated database password and TOTP key, once each (git-ignored)
	@test -f .env || (umask 077 && printf 'BRIER_DB_PASSWORD=%s\n' "$$(openssl rand -hex 24)" > .env && echo "created .env")
	@grep -q '^BRIER_TOTP_KEY=' .env || (printf 'BRIER_TOTP_KEY=%s\n' "$$(openssl rand -hex 32)" >> .env && echo "added BRIER_TOTP_KEY to .env")

up: env ## Start PostgreSQL in Docker and wait until it is healthy
	docker compose up -d --wait db

down: ## Stop the Docker services (data is kept in the volume)
	docker compose down

db-url: env ## Print the local database URL (for BRIER_DATABASE_URL)
	@. ./.env && echo "postgres://brier:$${BRIER_DB_PASSWORD}@127.0.0.1:55442/brier"

check: rust-check py-check private-check ## Lint, types and fast tests for everything (CI runs the same)

rust-check: ## cargo fmt, clippy with warnings as errors, Rust tests
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings
	$(CARGO) test --workspace --locked

py-check: ## ruff, mypy --strict, fast Python tests
	cd services && uv run --locked ruff format --check .
	cd services && uv run --locked ruff check .
	cd services && uv run --locked mypy
	cd services && uv run --locked pytest -q -m "not slow"

private-check: ## No tracked file mentions the private development tooling
	sh scripts/private_tooling_check.sh

differential-full: ## The gate study: 10 million random commands through both engines, full invariant checks every 100,000
	$(CARGO) build --release --locked -p brier-replay
	cd services && uv run --locked python -m brier_services.differential --binary ../target/release/brier-replay --seed $(SEED) --commands 10000000 --check-every 100000

crash: ## The registered crash study: 1,000 SIGKILLs with recovery checks (docs/notes/durability.md)
	$(CARGO) build --release --locked -p brier-core
	rm -rf target/crash-data
	cd services && uv run --locked python -m brier_services.crash --core ../target/release/brier-core --data ../target/crash-data --cycles 1000 --seed $(SEED)

bench: ## The registered P1/P2 engine benchmark (docs/notes/performance.md); plug in and close heavy apps first
	$(CARGO) build --release --locked -p brier-bench
	rustc --version
	-sw_vers -productVersion  # macOS only; ignored elsewhere
	-pmset -g batt | head -1  # macOS power source; ignored elsewhere
	./target/release/brier-bench

resolution-study: env ## The registered R1/R2 resolver study (docs/notes/resolution.md): downloads ~80 responses into var/study (network; FRED_API_KEY from .env)
	cd services && set -a && . ../.env && set +a && uv run --locked python -m brier_services.resolution.study fetch --dir ../var/study
	cd services && uv run --locked python -m brier_services.resolution.study run --dir ../var/study

fuzz-campaign: ## Every fuzz target for 1 hour, 3 at a time (needs nightly-2026-10-20 and cargo-fuzz); plug in first
	sh scripts/fuzz_campaign.sh

dependency-age: ## Every locked dependency was published at least 14 days ago (network: crates.io, PyPI)
	python3 scripts/dependency_age.py

gateway-bench: up ## The registered P3/P3b/P4 gateway benchmark (docs/notes/gateway.md); plug in and close heavy apps first
	$(CARGO) build --release --locked -p brier-bench
	rustc --version
	-sw_vers -productVersion  # macOS only; ignored elsewhere
	-pmset -g batt | head -1  # macOS power source; ignored elsewhere
	ulimit -n 10240 && BRIER_DATABASE_URL="$$($(MAKE) -s db-url)" ./target/release/brier-gateway-bench

test-all: check up ## Every test, including slow ones and the database tests (starts PostgreSQL)
	BRIER_TEST_DATABASE_URL="$$($(MAKE) -s db-url)" $(CARGO) test --workspace --locked -- --include-ignored
	cd services && BRIER_TEST_DATABASE_URL="$$($(MAKE) -s -C .. db-url)" uv run --locked pytest -q

audit: ## Known-vulnerability audit of locked Rust and Python dependencies
	@command -v cargo-audit >/dev/null 2>&1 || { echo "cargo-audit is not installed: run 'cargo install cargo-audit --locked' (CI always runs it)"; exit 1; }
	$(CARGO) audit
	mkdir -p target
	cd services && uv export --locked --all-groups --no-emit-project --format requirements-txt > ../target/requirements-audit.txt
	uvx --from pip-audit==$(PIP_AUDIT_VERSION) pip-audit --strict --requirement target/requirements-audit.txt
