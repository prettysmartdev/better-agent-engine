# launcher-core (Rust library) — component Makefile.
# Runs the toolchain directly; use the root Makefile to run this in Docker.
#
# launcher-core is a library with no binary of its own — it ships baked into the
# `baesched`/`baeapi` binaries built by launcher-schedule / launcher-api (which
# do the musl static-linking). Building it standalone with a plain `cargo build`
# is enough to verify it compiles and lints, so this mirrors the server's
# component Makefile rather than baectl's musl cross-compile.

.PHONY: help build test lint fmt clean

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

build: ## Compile the library
	cargo build

test: ## Run tests
	cargo test

lint: ## Clippy with warnings denied, plus a format check
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

fmt: ## Format the code
	cargo fmt

clean: ## Remove build artifacts
	cargo clean
