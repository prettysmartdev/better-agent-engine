# bae-py (Python) — component Makefile.
# Runs the toolchain (uv) directly; use the root Makefile to run this in Docker.

.PHONY: help install build test lint fmt clean publish

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

install: ## Sync the virtualenv (deps + dev deps)
	uv sync

build: ## Build sdist and wheel into dist/
	uv build

test: ## Run tests
	uv sync
	uv run --no-sync pytest

lint: ## Ruff checks and format check
	uv run ruff check
	uv run ruff format --check

fmt: ## Format the code
	uv run ruff format

clean: ## Remove build artifacts and the virtualenv
	rm -rf dist .venv .pytest_cache .ruff_cache src/bae_py/__pycache__ tests/__pycache__

publish: build ## Publish to PyPI (remove the Private classifier first)
	@echo "Built dist/. Run 'uv publish' manually to release."
