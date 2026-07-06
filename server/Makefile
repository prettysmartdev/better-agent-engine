# baesrv — component Makefile.
# Runs the toolchain directly; use the root Makefile to run this in Docker.

.PHONY: help build test lint fmt fmt-check clean run

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

build: ## Compile the server
	cargo build

test: ## Run unit and integration tests
	cargo test

lint: ## Clippy with warnings denied, plus a format check
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

fmt: ## Format the code
	cargo fmt

clean: ## Remove build artifacts
	cargo clean

run: ## Run the server (debug build)
	cargo run
