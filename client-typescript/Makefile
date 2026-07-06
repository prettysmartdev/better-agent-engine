# @base-engine/client (TypeScript) — component Makefile.
# Runs the toolchain directly; use the root Makefile to run this in Docker.

.PHONY: help install build test lint fmt clean publish

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

node_modules: package.json
	npm install
	@touch node_modules

install: node_modules ## Install dependencies

build: node_modules ## Compile TypeScript to dist/
	npm run build

test: node_modules ## Run tests
	npm test

lint: node_modules ## Type-check without emitting
	npm run lint
	npx prettier --check src

fmt: node_modules ## Format the code
	npx prettier --write src

clean: ## Remove build artifacts and dependencies
	rm -rf dist node_modules

publish: build ## Publish to npm (requires removing "private" from package.json)
	npm publish --dry-run
	@echo "Dry run OK. Run 'npm publish' manually to release."
