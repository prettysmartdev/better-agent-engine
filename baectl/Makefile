# baectl (Rust) — component Makefile.
# Runs the toolchain directly; use the root Makefile to run this in Docker.
#
# `build` cross-compiles the release artifact for the musl target so the binary
# is fully statically linked and drops into the `debian:bookworm-slim` runtime
# image with no libc/OpenSSL dependency — the same binary shape the production
# Dockerfile bundles. `test`/`lint`/`fmt` use the native host target (faster,
# no cross-compilation needed to run `cargo test`/`clippy`).

.PHONY: help build test lint fmt clean check-static

# Derive the musl target from the build host's architecture (x86_64 or aarch64)
# so the static binary is native to whatever machine runs the build — no x86-only
# assumption, and no cross-linker needed since target arch == host arch.
MUSL_TARGET := $(shell uname -m)-unknown-linux-musl

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

build: ## Compile the release static binary (musl target)
	cargo build --release --target $(MUSL_TARGET)

test: ## Run tests (native host target)
	cargo test

lint: ## Clippy with warnings denied, plus a format check
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

fmt: ## Format the code
	cargo fmt

clean: ## Remove build artifacts
	cargo clean

# Regression guard for the static-binary requirement. baectl ships as a fully
# static musl binary that drops into the dependency-free `debian:bookworm-slim`
# runtime image; a transitive OpenSSL/glibc dependency (e.g. a crate pulling in
# native-tls) would silently break that. This builds the musl target and asserts
# the result has no dynamic dependencies via both `file` and `ldd`. Run it inside
# the dev image, which provides the musl target + `musl-tools` (see Dockerfile.dev
# and the root `make check-static`).
check-static: ## Build the musl binary and assert it is statically linked
	cargo build --release --target $(MUSL_TARGET)
	@bin=target/$(MUSL_TARGET)/release/baectl; \
	echo "==> inspecting $$bin"; \
	out=$$(ldd "$$bin" 2>&1 || true); \
	echo "ldd: $$out"; \
	if echo "$$out" | grep -Eq "not a dynamic executable|statically linked"; then \
		echo "OK: ldd confirms no dynamic dependencies"; \
	else \
		echo "FAIL: baectl has dynamic dependencies (static-binary requirement regressed):" >&2; \
		echo "$$out" >&2; exit 1; \
	fi; \
	if command -v file >/dev/null 2>&1; then \
		file "$$bin"; \
		file "$$bin" | grep -q "statically linked" \
			&& echo "OK: file confirms statically linked" \
			|| { echo "FAIL: file does not report statically linked" >&2; exit 1; }; \
	else \
		echo "note: 'file' not installed; relied on ldd only"; \
	fi
