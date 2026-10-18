# vitals — common developer tasks. Run `make` (or `make help`) to list them.
# Prereqs: Rust toolchain (see docs/DEVELOPMENT.md).

# Database for dev mode. Isolated from a production deployment's DB.
# Override: make serve DB=/path.db
DB ?= $(HOME)/vitals-dev.db
# Health export to ingest with `make import` (gitignored if kept in the repo).
EXPORT ?= export.zip

.DEFAULT_GOAL := help
.PHONY: help server run serve prod import test check fmt lint clean

help: ## List available targets
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | sed 's/:.*## /\t/' \
		| awk -F'\t' '{printf "  make %-7s %s\n", $$1, $$2}'

server: ## Build the release server binary
	cargo build --release -p vitals-server

run: serve ## Alias for serve (no frontend to rebuild)

serve: ## Serve /api against $(DB) on 127.0.0.1:8090 (honours VITALS_DB_KEY[_FILE] with encryption)
	VITALS_DB="$(DB)" cargo run --features encryption -p vitals-server

prod: ## Production deploy: build encrypted release binaries, then restart the launchd daemon
	cargo build --release --features encryption -p vitals-server
	cargo build --release --features encryption,client -p vitals-cli
	sudo launchctl kickstart -k system/com.vitals.server

import: ## Import $(EXPORT) into $(DB) (idempotent)
	cargo run --release --features encryption -p vitals-cli -- --db "$(DB)" import "$(EXPORT)"

test: ## Run the test suites
	cargo test --workspace --features vitals-cli/client

check: ## Fast pre-commit gate: format check, clippy, tests
	cargo fmt --check
	cargo clippy --workspace --all-targets --features vitals-cli/client -- -D warnings
	cargo test --workspace --features vitals-cli/client

fmt: ## Format all Rust code
	cargo fmt

lint: ## Clippy across the workspace, warnings as errors
	cargo clippy --workspace --all-targets --features vitals-cli/client -- -D warnings

clean: ## Remove cargo build artifacts
	cargo clean
