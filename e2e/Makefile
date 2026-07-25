# bae-e2e — component Makefile.
# Runs the toolchain directly; use the root Makefile to run this in Docker.
#
# Unlike every other component, the tests here drive the real `baesrv` binary as
# a child process rather than linking the code under test. `test` therefore
# builds that binary first and hands the suite its path via BAE_E2E_BAESRV.
# The build writes into server/target/ — the same directory `make test-server`
# uses — so the two share a compilation cache instead of duplicating one.

BAESRV_MANIFEST := ../server/Cargo.toml
BAESRV_BIN      := $(abspath ../server/target/debug/baesrv)

.PHONY: help build test lint fmt clean

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

build: ## Compile the test crate (compiles the suite; runs nothing)
	cargo build --all-targets

test: ## Build baesrv, then run the end-to-end suite against it
	cargo build --manifest-path $(BAESRV_MANIFEST) --bin baesrv
	BAE_E2E_BAESRV=$(BAESRV_BIN) cargo test

lint: ## Clippy with warnings denied, plus a format check
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

fmt: ## Format the code
	cargo fmt

clean: ## Remove build artifacts
	cargo clean
