# bae-launcher-webapp — component shim. The only buildable/testable piece of
# this launcher is the Vite + React frontend in web/ (the Rust side is the
# unmodified `baeapi` from launchers/api/), so every standard verb delegates
# there. This shim exists so the root Makefile's COMPONENTS entry is
# `launchers/webapp`, giving the documented `<verb>-launchers/webapp` targets
# (work item 0014 section E).

.PHONY: help build test lint fmt clean

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

build: ## Build the frontend (web/)
	$(MAKE) -C web build

test: ## Test the frontend (web/)
	$(MAKE) -C web test

lint: ## Lint the frontend (web/)
	$(MAKE) -C web lint

fmt: ## Format the frontend (web/)
	$(MAKE) -C web fmt

clean: ## Clean the frontend (web/)
	$(MAKE) -C web clean
