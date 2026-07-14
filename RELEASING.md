# Releasing BAE

BAE ships **one unified version per release**: a single `vX.Y.Z` stamps all three
client SDKs and both Docker images. Cutting a release is one command from a
maintainer's machine, plus a CI job that publishes the images.

```
 ┌ prepare ─────────────┐   ┌ make release VERSION=vX.Y.Z ─────────────┐   ┌ CI (on the tag) ──────────┐
 │ /release-prep skill: │   │ scripts/release.sh:                       │   │ .github/workflows/        │
 │  • bump versions     │ → │  • validate (make lint + make test)       │ → │  release.yml:             │
 │  • update docs/notes │   │  • publish SDKs (crates.io/npm/PyPI)      │   │  • build both images,     │
 │  (does NOT release)  │   │  • push commit + vX.Y.Z tag ──────────────┼──▶│    multi-arch amd64+arm64 │
 └──────────────────────┘   └───────────────────────────────────────────┘   │  • push to GHCR           │
                                                                             └───────────────────────────┘
```

- **SDKs** are published locally by `make release` using **your** crates.io / npm /
  PyPI credentials — never from CI, no registry secrets in GitHub.
- **Images** are published by CI when the `vX.Y.Z` tag lands, using the workflow's
  `GITHUB_TOKEN`.

The whole flow is idempotent — every step checks whether it's already done and
skips it — so re-running after a failure is safe, and any failing step offers to
skip and continue.

---

## Credentials you need (every release)

| Registry | How the script authenticates |
| --- | --- |
| crates.io | `cargo login`, or `CARGO_REGISTRY_TOKEN` in your environment |
| npm | `npm login`, or a `~/.npmrc` authToken / `NODE_AUTH_TOKEN` |
| PyPI | `UV_PUBLISH_TOKEN` in your environment, or `~/.pypirc` |
| GitHub | `gh auth login` (for the GitHub release; the image build uses CI's own token) |

You also need `cargo`, `npm`, `uv`, `git`, and `curl` on your PATH. Validation
(`make lint` / `make test`) runs through Docker if it's present, or falls back to
your host toolchains.

---

## First release — one-time setup

Do these **once**, before the very first `make release`. They are deliberately
**not** automated (the release script assumes they're already done).

### 1. Remove the "do not publish" markers

Each SDK manifest ships un-publishable until the first release. Remove the marker
in each and commit:

- **`client-rust/Cargo.toml`** — delete the line:
  ```toml
  publish = false
  ```
- **`client-typescript/package.json`** — set it to false (or remove the key):
  ```json
  "private": false,
  ```
- **`client-python/pyproject.toml`** — delete the classifier line:
  ```toml
  classifiers = ["Private :: Do Not Upload"]
  ```

> `baectl`, `max/web`, and `max/server` keep their markers **permanently** — they
> ship only inside the Docker images and are never published to a registry. Don't
> touch those.

### 2. Claim the package names

Make sure the names are registrable/owned by your accounts before you publish:

- **crates.io** — `bae-rs` is available or owned by your crates.io account.
- **npm** — you can publish to the `@prettysmartdev` scope, and the first publish
  uses `npm publish --access public` (the script already passes `--access public`).
- **PyPI** — the `bae-py` project exists under your account, or your token is
  allowed to create it on first upload.

### 3. Enable arm64 image builds

`release.yml` builds arm64 on GitHub's native `ubuntu-24.04-arm` runners.

- **Public repo:** arm64 hosted runners are available at no cost — nothing to do.
- **Private repo:** enable the arm64 runners for the repo/org (they require a plan
  that includes them). If they're unavailable, the arm64 `build` matrix legs won't
  find a runner.

### 4. After the first image push: make the GHCR package public

GHCR packages are created **private** on first push. Anonymous `docker pull` fails
until you make them public:

1. GitHub → your org/user **Packages** → `better-agent-engine`.
2. **Package settings** → **Danger Zone** → **Change visibility** → **Public**.
3. Under **Manage Actions access**, confirm the repository has **Write** access so
   future releases can push new tags.

> **Tip:** to auto-link the package to this repo (so it inherits repo settings and
> shows on the repo sidebar), add an OCI source label to both Dockerfiles:
> ```dockerfile
> LABEL org.opencontainers.image.source=https://github.com/prettysmartdev/better-agent-engine
> ```

### 5. Confirm the workflow token can push packages

`release.yml` already declares `permissions: packages: write`. If your org
restricts the default `GITHUB_TOKEN`, allow Actions to write packages under
**Settings → Actions → General → Workflow permissions**.

---

## Cutting a release (every time)

1. **Prepare.** Run the `release-prep` skill (e.g. `/release-prep v0.1.0`) — it sets
   the version across the three SDK manifests, updates README/docs for anything new,
   and drafts release notes. Review and commit the result. It does **not** publish
   or tag.
2. **Release.** From a clean `main`:
   ```sh
   make release VERSION=v0.1.0
   ```
   This validates locally, publishes the three SDKs with your credentials, and
   pushes the `v0.1.0` commit + tag. Answer the per-step prompts; skip any step that
   fails if you want to handle it manually.
3. **Watch the images build.** The pushed tag triggers `release.yml`, which builds
   both images (amd64 + arm64) and pushes them to GHCR. Track it at
   **Actions → release**.

---

## Verify

```sh
# Images — the manifest should list both linux/amd64 and linux/arm64.
docker buildx imagetools inspect ghcr.io/prettysmartdev/better-agent-engine:0.1.0
docker buildx imagetools inspect ghcr.io/prettysmartdev/better-agent-engine:0.1.0-max
docker pull ghcr.io/prettysmartdev/better-agent-engine:latest
docker pull ghcr.io/prettysmartdev/better-agent-engine:max

# SDKs
cargo add bae-rs@0.1.0
npm view @prettysmartdev/bae-ts@0.1.0 version
pip index versions bae-py    # or: uv pip install bae-py==0.1.0
```

Published GHCR tags:

| Image | Floating | Pinned |
| --- | --- | --- |
| server | `:latest` | `:<semver>` |
| bae-max | `:max` | `:<semver>-max` |

Floating tags (`:latest`, `:max`) only move for **stable** versions — a prerelease
like `v0.2.0-rc1` publishes only `:0.2.0-rc1` and `:0.2.0-rc1-max`.

---

## If something fails

`make release` is idempotent — just fix the problem and run it again. It will:

- skip versions already set and SDK versions already live on a registry (registry
  versions are immutable, so it never double-publishes);
- skip the tag if it's already on origin, and the GitHub release if it already
  exists;
- re-run only what's left.

If an **image** build fails after the tag is already pushed, fix the cause and
re-run the workflow from **Actions → release → Re-run jobs** (the tag doesn't need
to move). The manifest merge is safe to repeat.

See [DEVELOPING.md](DEVELOPING.md) for building and testing from source.
