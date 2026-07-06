# Better Agent Engine (BAE) — root orchestration.
#
# All local development runs inside the Docker dev image (Dockerfile.dev),
# with the repository bind-mounted at /workspace. Component Makefiles
# (server/, client-rust/, client-typescript/, client-python/) invoke the
# toolchains directly and are what actually run inside the container.
#
# Typical loop:
#   make dev-image        # build the dev toolchain image (once, or after edits)
#   make test             # run every component's tests in the container
#   make test-server      # run one component's tests
#   make shell            # interactive shell in the dev container

PROJECT    := better-agent-engine
DEV_IMAGE  ?= awman-$(PROJECT):latest
IMAGE      ?= $(PROJECT):latest
COMPONENTS := server client-rust client-typescript client-python
PORT       ?= 8080

# Allocate a TTY only when we have one (keeps CI happy).
TTY := $(shell [ -t 0 ] && echo --interactive --tty)

# Named volume for the cargo registry so Rust dependency downloads survive
# container restarts. Everything else (target/, node_modules/, .venv/) lives
# in the bind-mounted workspace and is cached there naturally.
DOCKER_RUN := docker run --rm $(TTY) \
	--volume $(CURDIR):/workspace \
	--volume $(PROJECT)-cargo-registry:/usr/local/cargo/registry \
	--workdir /workspace \
	$(DEV_IMAGE)

# Note: the per-component <verb>-<component> targets are pattern rules and
# intentionally NOT declared .PHONY — make ignores pattern rules for .PHONY
# targets.
.PHONY: help dev-image ensure-dev-image shell image run \
	build test lint fmt clean

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-18s %s\n", $$1, $$2}'
	@echo
	@echo "  <verb>-<component>  Run one verb for one component, e.g. test-server,"
	@echo "                      lint-client-python. Components: $(COMPONENTS)"

dev-image: ## Build the Docker dev toolchain image
	docker build --file Dockerfile.dev --tag $(DEV_IMAGE) .

ensure-dev-image:
	@docker image inspect $(DEV_IMAGE) >/dev/null 2>&1 || $(MAKE) dev-image

shell: ensure-dev-image ## Interactive shell inside the dev container
	$(DOCKER_RUN) bash

image: ## Build the production server image (Dockerfile)
	docker build --file Dockerfile --tag $(IMAGE) .

run: ensure-dev-image ## Run the server in the dev container (port $(PORT))
	docker run --rm $(TTY) \
		--volume $(CURDIR):/workspace \
		--volume $(PROJECT)-cargo-registry:/usr/local/cargo/registry \
		--workdir /workspace \
		--publish $(PORT):$(PORT) \
		$(DEV_IMAGE) make -C server run

build: ## Build every component
test: ## Test every component
lint: ## Lint every component
fmt: ## Format every component
clean: ## Clean every component's build artifacts
build test lint fmt clean: ensure-dev-image
	$(DOCKER_RUN) bash -ec 'for c in $(COMPONENTS); do echo "==> $$c: $@"; make -C $$c $@; done'

# Per-component verbs: make <verb>-<component>, e.g. `make test-client-rust`.
build-%: ensure-dev-image
	$(DOCKER_RUN) make -C $* build
test-%: ensure-dev-image
	$(DOCKER_RUN) make -C $* test
lint-%: ensure-dev-image
	$(DOCKER_RUN) make -C $* lint
fmt-%: ensure-dev-image
	$(DOCKER_RUN) make -C $* fmt
clean-%: ensure-dev-image
	$(DOCKER_RUN) make -C $* clean
