# Continuous Integration and Deployment

Platform: github

## Pipelines:

Build & Test (`.github/workflows/test.yml`):
- On every push (any branch) and every PR, build/lint/test all components using the same entrypoints as local dev: build the dev image (buildx + GHA layer cache, tagged as the Makefile's `DEV_IMAGE` so `ensure-dev-image` reuses it), then `make build`, `make lint`, `make test` — CI never invents steps that local dev doesn't have.
- One job runs the full suite across all components (`server/`, `baectl/`, `client-rust/`, `client-typescript/`, `client-python/`, `max/`). Per-component path filtering is a possible future optimization, not yet implemented.
- `make test`/`make lint` must pass before merge. All tests are offline (no real providers/network).
- The workflow also exposes `workflow_call` so the release workflow can reuse it as a gate.

Releases:
- **One unified version per release**, not per-component. A release is cut by a maintainer running `make release VERSION=vX.Y.Z` from their machine (see `scripts/release.sh`). That one command: sets the version across all three client SDK manifests, re-syncs lockfiles, validates locally (`make lint` + `make test`), **publishes the three client SDKs from the maintainer's machine using their local registry credentials**, then pushes the single `vX.Y.Z` git tag. The script is idempotent (skips work already done — version already set, package version already live on the registry, tag already pushed) and offers to skip any failing step.
- Pushing the `vX.Y.Z` tag triggers `.github/workflows/release.yml`, which builds and pushes **both** Docker images to GHCR (see Publishing). SDK publishing happens locally in `make release`; image publishing happens in CI on the tag.
- Each GitHub release gets notes auto-generated from merged PR titles since the previous tag (`gh release create --generate-notes`, invoked by the release script).

Versioning:
- **All components share one SemVer line**, bumped together and released as a single `vX.Y.Z` tag — the three SDKs and both images all carry that version. The API version (`/api/v1`) remains the wire-compatibility contract; independently built and installed client/server binaries interoperate across patch/minor versions per that contract.
- Pre-1.0, minor bumps may include breaking changes; from 1.0 on, strict SemVer.

Publishing:
- **Client SDKs are published locally, not from CI.** `make release` runs `cargo publish` (client-rust → crates.io, `bae-rs`), `npm publish --access public` (client-typescript → npm, `@prettysmartdev/bae-ts`), and `uv build && uv publish` (client-python → PyPI, `bae-py`) on the maintainer's machine, using their local crates.io/npm/PyPI credentials. No registry secrets live in GitHub Actions. Each publish first checks the registry for that exact version and skips if already live (published versions are immutable, so a re-run never double-publishes).
- **Images are published from CI on the tag**, multi-arch (`linux/amd64` + `linux/arm64`). `release.yml` builds each architecture on its own native runner (`ubuntu-latest` for amd64, `ubuntu-24.04-arm` for arm64 — no QEMU emulation; the Dockerfiles derive their Rust target from `uname -m` and build correctly on each host), pushes each by digest, then merges the per-arch digests into one manifest list per image. Authenticated with the workflow's `GITHUB_TOKEN` (`packages: write`); no external secrets.
  - server → `ghcr.io/prettysmartdev/better-agent-engine:<semver>` and `:latest`, from `Dockerfile`.
  - max (`max/`) → **same GHCR repository, suffixed tags** `:<semver>-max` and `:max` (e.g. `ghcr.io/prettysmartdev/better-agent-engine:0.3.0-max`, `:max`), from `Dockerfile.max`, on the same tag push. One image name to remember; `-max` is the variant that includes the dashboard.
  - The floating `:latest` / `:max` tags only move for stable versions — a prerelease tag (e.g. `v0.3.0-rc1`) publishes only the exact `:<semver>` / `:<semver>-max` tags.
- baectl has **no publish of its own**: not on crates.io/npm/PyPI, it ships only as the static binary baked into both images, so its effective version tracks the image tag. Its `Cargo.toml` carries `publish = false` permanently. Likewise `max/web` and `max/server` carry `"private": true` permanently — max is delivered only as the image variant, never to npm.
- The three publishable SDKs keep their private/no-publish markers (`publish = false`, `"private": true`, the `Private :: Do Not Upload` classifier) until the first release, at which point a maintainer removes them **manually**, once. The release script assumes the SDKs are already publish-ready and never edits those markers.

Deployment:
- The deliverable is the published Docker image; operators pull and run it themselves (see devops/operations.md). No hosted environment is deployed from CI yet.
