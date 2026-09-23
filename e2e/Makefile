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

# The engine-gated suites (tests/quickstart.rs, tests/sdk_containers.rs) drive
# the *host* baectl against a real Docker engine, so they run on the host, not
# in the dev image. They need `make image` (and, for test-engine,
# `make image-launcher-api`) at the repository root first. The gate env makes
# them run; BAE_E2E_ENGINE_REQUIRED turns a missing prerequisite into a failure
# instead of a skip. Plain `test` leaves both unset, so they skip cleanly.
BAECTL_BIN ?= $(abspath ../baectl/target/host/release/baectl)
ENGINE_ENV := BAECTL_HARNESS_ENGINE_TESTS=1 BAE_E2E_ENGINE_REQUIRED=1 BAECTL_BIN=$(BAECTL_BIN)

.PHONY: help build test test-quickstart test-engine baectl-host lint fmt clean

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

build: ## Compile the test crate (compiles the suite; runs nothing)
	cargo build --all-targets

test: ## Build baesrv, then run the end-to-end suite against it
	cargo build --manifest-path $(BAESRV_MANIFEST) --bin baesrv
	BAE_E2E_BAESRV=$(BAESRV_BIN) cargo test

baectl-host:
	$(MAKE) -C ../baectl build-host

test-quickstart: baectl-host ## Run the docs quickstart smoke test (needs Docker + `make image`)
	$(ENGINE_ENV) cargo test --test quickstart -- --nocapture

test-engine: baectl-host ## Per-SDK API-container round trips (needs Docker + images)
	$(ENGINE_ENV) cargo test --test sdk_containers -- --nocapture --test-threads=1

lint: ## Clippy with warnings denied, plus a format check
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

fmt: ## Format the code
	cargo fmt

clean: ## Remove build artifacts
	cargo clean
