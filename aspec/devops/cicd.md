# Continuous Integration and Deployment

Platform: github

## Pipelines:

Build:
- On every PR and push to main, build all four components using the same entrypoints as local dev: build the dev image (cached), then `make build` — CI must never invent steps that local dev doesn't have.
- Use per-component paths filters so a client-only change doesn't rebuild the server image.

Test:
- `make test` and `make lint` run on every PR; both must pass before merge.
- Integration tests (server + each client) run on PRs that touch the server or the API surface.

Releases:
- Components release independently. A release is a git tag of the form `<component>-v<semver>` (e.g. `server-v0.2.0`, `client-python-v0.1.3`) pushed by a maintainer; the tag triggers that component's publish job.
- Each release gets GitHub Release notes generated from merged PR titles since the previous tag of that component.

Versioning:
- Independent SemVer per component. The API version (`/api/v1`) is the compatibility contract between them — client and server versions do not need to match.
- Pre-1.0, minor bumps may include breaking changes; from 1.0 on, strict SemVer.

Publishing:
- server → Docker image (GHCR) tagged `<semver>` and `latest`, built from the root Dockerfile.
- client-rust → crates.io (`bae-rs`); client-typescript → npm (`@prettysmartdev/bae-ts`); client-python → PyPI (`bae-py`, via uv build/publish).
- Registry credentials live in GitHub Actions secrets; publish jobs run only on tags. Each package manifest keeps its private/no-publish marker until its first release is cut.

Deployment:
- The deliverable is the published Docker image; operators pull and run it themselves (see devops/operations.md). No hosted environment is deployed from CI yet.
