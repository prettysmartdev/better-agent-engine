# bae-launcher-webapp's Vite + React frontend. Kept independently buildable,
# like max/web, because the image consumes only the resulting static dist/.

.PHONY: help build test lint fmt clean

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

node_modules: package.json package-lock.json
	npm ci
	@touch node_modules

build: node_modules ## Type-check and build dist/
	npm run build

test: node_modules ## Run the frontend test command
	npm run test

lint: node_modules ## Lint and check formatting
	npm run lint

fmt: node_modules ## Format source files
	npm run fmt

clean: ## Remove build artifacts and dependencies
	rm -rf dist node_modules
