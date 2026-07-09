# Better Agent Engine (BAE) — root orchestration.
#
# All local development runs inside the containerized dev image (Dockerfile.dev),
# with the repository bind-mounted at /workspace. Component Makefiles
# (server/, client-rust/, client-typescript/, client-python/) invoke the
# toolchains directly and are what actually run inside the container.
#
# Container engine: Docker is used when its CLI exists and the daemon responds.
# Otherwise, if Apple's `container` CLI (https://github.com/apple/container)
# is installed, every target transparently falls back to Apple containers.
#
# Typical loop:
#   make dev-image        # build the dev toolchain image (once, or after edits)
#   make test             # run every component's tests in the container
#   make test-server      # run one component's tests
#   make shell            # interactive shell in the dev container

PROJECT    := better-agent-engine
DEV_IMAGE  ?= awman-$(PROJECT):latest
IMAGE      ?= $(PROJECT):latest
COMPONENTS := server baectl client-rust client-typescript client-python max
PORT       ?= 8080

# Pick the container engine: docker if the CLI exists and the daemon is up,
# else Apple containers, else empty (container targets fail via ensure-engine).
# ENGINE=docker|container can also be forced on the command line.
ifeq ($(origin ENGINE), undefined)
ifeq ($(shell command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1 && echo ok),ok)
ENGINE := docker
else ifeq ($(shell command -v container >/dev/null 2>&1 && echo ok),ok)
ENGINE := container
else
ENGINE :=
endif
endif

# Allocate a TTY only when we have one (keeps CI happy).
TTY := $(shell [ -t 0 ] && echo --interactive --tty)

# Named volume for the cargo registry so Rust dependency downloads survive
# container restarts. Everything else (target/, node_modules/, .venv/) lives
# in the bind-mounted workspace and is cached there naturally.
CARGO_VOLUME := $(PROJECT)-cargo-registry

# Named volume for the server's SQLite data (default BAE_DB_PATH is
# /var/lib/bae/bae.db) so `make run` has a writable DB directory and the
# data survives the --rm container.
DATA_VOLUME := $(PROJECT)-dev-data

CONTAINER_RUN := $(ENGINE) run --rm $(TTY) \
	--volume $(CURDIR):/workspace \
	--volume $(CARGO_VOLUME):/usr/local/cargo/registry \
	--workdir /workspace \
	$(DEV_IMAGE)

# Runner for the component verbs (build/test/lint/fmt/clean and their
# <verb>-<component> variants). With a container engine these run inside the dev
# image — the canonical, reproducible path. With no engine available they fall
# back to the host toolchain so `make test` (and the sibling verbs) still work in
# engine-less environments such as CI sandboxes and remote workers, where the
# component Makefiles run directly. That fallback requires the toolchains the dev
# image bundles (cargo, npm, uv) to be present on the host. The image-centric
# targets (shell/image/run/dev-image/image-smoke/check-static) have no host
# equivalent and still hard-require an engine via ensure-engine.
ifeq ($(ENGINE),)
DEV_IMAGE_DEP :=
RUN_IN_DEV    :=
else
DEV_IMAGE_DEP := ensure-dev-image
RUN_IN_DEV    := $(CONTAINER_RUN)
endif

# Note: the per-component <verb>-<component> targets are pattern rules and
# intentionally NOT declared .PHONY — make ignores pattern rules for .PHONY
# targets.
.PHONY: help engine dev-image ensure-engine ensure-dev-image shell image image-max run \
	build test lint fmt clean check-static image-smoke

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-18s %s\n", $$1, $$2}'
	@echo
	@echo "  <verb>-<component>  Run one verb for one component, e.g. test-server,"
	@echo "                      lint-client-python. Components: $(COMPONENTS)"

engine: ## Show which container engine make will use
	@echo $(if $(ENGINE),$(ENGINE),none)

# Fail early with a clear message when no engine is usable. For Apple
# containers, also start its services and pre-create the named volumes
# (Docker auto-creates named volumes on `run`; `container` does not).
ensure-engine:
ifeq ($(ENGINE),)
	@echo "error: no container engine available." >&2
	@echo "       Install Docker (and start its daemon), or Apple containers" >&2
	@echo "       (https://github.com/apple/container) on macOS." >&2
	@exit 1
endif
ifeq ($(ENGINE),container)
	@container system status >/dev/null 2>&1 || container system start
	@for v in $(CARGO_VOLUME) $(DATA_VOLUME); do \
		container volume inspect $$v >/dev/null 2>&1 || container volume create $$v; \
	done
endif

dev-image: ensure-engine ## Build the dev toolchain image
	$(ENGINE) build --file Dockerfile.dev --tag $(DEV_IMAGE) .

ensure-dev-image: ensure-engine
	@$(ENGINE) image inspect $(DEV_IMAGE) >/dev/null 2>&1 || $(MAKE) dev-image

shell: ensure-dev-image ## Interactive shell inside the dev container
	$(CONTAINER_RUN) bash

image: ensure-engine ## Build the production server image (Dockerfile)
	$(ENGINE) build --file Dockerfile --tag $(IMAGE) .

image-max: ensure-engine ## Build the bae-max image variant (Dockerfile.max)
	$(ENGINE) build --file Dockerfile.max --tag $(IMAGE)-max .

# Static-binary regression guard for baectl. Runs inside the dev image, which
# carries the x86_64-unknown-linux-musl target + musl-tools, and asserts the
# release binary links statically (no transitive OpenSSL/glibc). Catches a
# dependency regression that would break the dependency-free runtime image.
check-static: ensure-dev-image ## Verify baectl builds as a static musl binary
	$(CONTAINER_RUN) make -C baectl check-static

# Image smoke test: build the production image and run the bundled baectl in it.
# The runtime base carries no Rust toolchain, so `baectl --help` succeeding proves
# the shipped binary is genuinely self-contained/static.
image-smoke: image ## Build the image and run `baectl --help` in it
	$(ENGINE) run --rm $(IMAGE) baectl --help

# Named so the loopback-only admin API is reachable via exec, e.g.:
#   docker exec better-agent-engine-dev curl -s http://127.0.0.1:8081/admin/v1/keys …
# (substitute `container` for `docker` on Apple containers). The provider key
# is forwarded because profiles reference it server-side (`${ANTHROPIC_API_KEY}`);
# expanded by the shell at run time so the value is never echoed by make.
run: ensure-dev-image ## Run the server in the dev container (port $(PORT))
	$(ENGINE) run --rm $(TTY) \
		--name $(PROJECT)-dev \
		--volume $(CURDIR):/workspace \
		--volume $(CARGO_VOLUME):/usr/local/cargo/registry \
		--volume $(DATA_VOLUME):/var/lib/bae \
		--workdir /workspace \
		--publish $(PORT):$(PORT) \
		--env ANTHROPIC_API_KEY="$$ANTHROPIC_API_KEY" \
		$(DEV_IMAGE) make -C server run

build: ## Build every component
test: ## Test every component
lint: ## Lint every component
fmt: ## Format every component
clean: ## Clean every component's build artifacts
build test lint fmt clean: $(DEV_IMAGE_DEP)
	$(RUN_IN_DEV) bash -ec 'for c in $(COMPONENTS); do echo "==> $$c: $@"; make -C $$c $@; done'

# Per-component verbs: make <verb>-<component>, e.g. `make test-client-rust`.
build-%: $(DEV_IMAGE_DEP)
	$(RUN_IN_DEV) make -C $* build
test-%: $(DEV_IMAGE_DEP)
	$(RUN_IN_DEV) make -C $* test
lint-%: $(DEV_IMAGE_DEP)
	$(RUN_IN_DEV) make -C $* lint
fmt-%: $(DEV_IMAGE_DEP)
	$(RUN_IN_DEV) make -C $* fmt
clean-%: $(DEV_IMAGE_DEP)
	$(RUN_IN_DEV) make -C $* clean
