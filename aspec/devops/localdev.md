# Local Development

Development: docker
Build tools: make (orchestration), cargo, npm, uv (per component, inside the dev container)

## Workflows:

Developer Loop:
- Host requirements are only `docker` and `make`. Build the dev toolchain image once with `make dev-image` (Dockerfile.dev: Rust stable, Node 22, Python 3 + uv); the root Makefile bind-mounts the repo at /workspace and runs everything inside it.
- Whole-repo verbs: `make build|test|lint|fmt|clean` loop over all four components. Single-component verbs: `make <verb>-<component>`, e.g. `make test-server`, `make lint-client-python`.
- `make shell` opens an interactive shell in the dev container; inside it, work directly in a component with `make -C <component> <verb>`.
- `make run` starts the server in the dev container with the port published; `make image` builds the production server image.

Local testing:
- Every component has unit tests runnable via its `test` target (cargo test / vitest / pytest); run them before every commit via `make test`.
- Integration tests (as they land) run a real server against a throwaway SQLite file and exercise it through the client libraries.
- Lint is part of the definition of done: `make lint` runs clippy -D warnings + rustfmt --check, tsc --noEmit + prettier --check, and ruff.

Version control:
- Trunk-based on `main`: short-lived feature branches, PRs reviewed before merge, no direct pushes to main.
- Branch names `feature/<work-item>-<slug>` tied to an aspec work item; commit messages in imperative mood with a scope prefix (e.g. `server:`, `client-python:`).
- Never commit secrets, `.env` files, or SQLite databases (enforced by .gitignore).

Documentation:
- The aspec/ tree is the source of truth for design and process; update the relevant aspec file in the same PR as the change it describes.
- Each component keeps a README covering its own develop/publish loop; the root README covers repo-wide workflows.
- New feature work starts as a work item in aspec/work-items/ copied from 0000-template.md, numbered sequentially.
