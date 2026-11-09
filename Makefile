# vitals — common developer tasks. Run `make` (or `make help`) to list them.
# Prereqs: Rust toolchain (see docs/DEVELOPMENT.md).

# Database for dev mode. Isolated from a production deployment's DB.
# Override: make serve DB=/path.db
DB ?= $(HOME)/vitals-dev.db
# Health export to ingest with `make import` (gitignored if kept in the repo).
EXPORT ?= export.zip

.DEFAULT_GOAL := help
.PHONY: help server run serve deploy-build deploy import dev-db test check leakscan release-draft release smoke fmt lint clean

help: ## List available targets
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | sed 's/:.*## /\t/' \
		| awk -F'\t' '{printf "  make %-13s %s\n", $$1, $$2}'

server: ## Build the release server binary
	cargo build --release -p vitals-server

run: serve ## Alias for serve (no frontend to rebuild)

serve: ## Serve /api against $(DB) on 127.0.0.1:8090 (honours VITALS_DB_KEY[_FILE] with encryption)
	VITALS_DB="$(DB)" cargo run --features encryption,oura -p vitals-server

deploy-build: ## Build release TAG for deploy, no sudo: make deploy-build TAG=vYYYY.MM.N (scripts/deploy.sh)
	scripts/deploy.sh build $(TAG)

deploy: ## Install built release TAG and restart the server (sudo: run in a terminal)
	scripts/deploy.sh install $(TAG)

import: ## Import $(EXPORT) into $(DB) (idempotent)
	cargo run --release --features encryption -p vitals-cli -- --db "$(DB)" import "$(EXPORT)"

dev-db: ## Rebuild $(DB) from synthetic data with edge cases (DAYS=, SEED=, END=); no real data
	DB="$(DB)" scripts/dev-db.sh

test: ## Run the test suites
	cargo test --workspace --features vitals-cli/client,vitals-cli/oura,vitals-server/oura

check: ## Pre-commit gate: format check, clippy, tests, leak scan
	cargo fmt --check
	cargo clippy --workspace --all-targets --features vitals-cli/client,vitals-cli/oura,vitals-server/oura -- -D warnings
	cargo test --workspace --features vitals-cli/client,vitals-cli/oura,vitals-server/oura
	$(MAKE) --no-print-directory leakscan

release-draft: ## Draft the next release's notes at the top of CHANGELOG.md (git-cliff); edit them, then make release
	python3 -I scripts/release.py draft

release: check ## Gate, scan the drafted notes, commit them and tag the release (pushes nothing)
	python3 -I scripts/release.py cut

leakscan: ## Scan all history for personal data (shape rules, plus a denylist from LEAKSCAN_DENYLIST_FILE)
	python3 -I -m unittest discover -s scripts -p 'test_*.py'
	python3 -I scripts/leakscan.py --all

fmt: ## Format all Rust code
	cargo fmt

lint: ## Clippy across the workspace, warnings as errors
	cargo clippy --workspace --all-targets --features vitals-cli/client,vitals-cli/oura,vitals-server/oura -- -D warnings

clean: ## Remove cargo build artifacts
	cargo clean

smoke: ## End-to-end API check against $(DB): auth tiers, every route, CLI --server == local (spare port 8091)
	DB="$(DB)" scripts/smoke.sh
