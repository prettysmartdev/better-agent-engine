# max (Node.js/TypeScript) — component Makefile.
# Runs the toolchain directly; use the root Makefile to run this in Docker.
#
# max/ is two independently-installed npm packages, web/ (the Vite + React
# frontend) and server/ (the Express + ws backend that serves web/'s build
# output). Each verb below just runs both sub-packages' own npm script.

.PHONY: help build test lint fmt clean

help: ## Show available targets
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

web/node_modules: web/package.json
	cd web && npm install
	@touch web/node_modules

server/node_modules: server/package.json
	cd server && npm install
	@touch server/node_modules

build: web/node_modules server/node_modules ## Build web/'s dist/ and compile server/ to JS
	cd web && npm run build
	cd server && npm run build

test: web/node_modules server/node_modules ## Run both sub-packages' tests, plus the bae-max entrypoint regression test
	cd web && npm test
	cd server && npm test
	../docker/bae-max-entrypoint.test.sh

lint: web/node_modules server/node_modules ## Type-check and format-check both sub-packages
	cd web && npm run lint
	cd server && npm run lint

fmt: web/node_modules server/node_modules ## Format both sub-packages
	cd web && npm run fmt
	cd server && npm run fmt

clean: ## Remove build artifacts and dependencies
	rm -rf web/dist web/node_modules server/dist server/node_modules
