#!/usr/bin/env bash
# scripts/release.sh — idempotent release orchestration for Better Agent Engine (BAE).
#
# Usage: scripts/release.sh vX.Y.Z      (normally: make release VERSION=vX.Y.Z)
#
# Cuts a full BAE release from your machine, in order:
#   1. Pre-flight checks         — version format, gh auth, git branch/sync/tree
#   2. Prepare SDK manifests     — set the version across all three client SDKs and
#                                  commit the result (assumes they are already
#                                  publish-ready; it never edits publish/private markers)
#   3. Validate locally          — `make lint` + `make test`
#   4. Publish the client SDKs    — bae-rs → crates.io, @prettysmartdev/bae-ts → npm,
#                                  bae-py → PyPI, using YOUR LOCAL credentials
#   5. Push commit + tag         — pushing the vX.Y.Z tag triggers the GitHub Actions
#                                  workflow that builds and pushes both the standard and
#                                  bae-max images to GHCR
#   6. GitHub release            — with auto-generated notes
#
# Idempotency: every step first checks whether it is already done — version already
# set, crate/package/tag already live on the registry, GitHub release already created —
# and skips rather than repeating work. Re-running after a failure is safe and only
# does what remains.
#
# Skipping: any failing step offers to skip it and continue the release (or abort).
#
# Credentials (all local, never read from CI):
#   crates.io  — `cargo login`, or CARGO_REGISTRY_TOKEN in the environment
#   npm        — `npm login`, or a ~/.npmrc authToken / NODE_AUTH_TOKEN
#   PyPI       — UV_PUBLISH_TOKEN in the environment, or ~/.pypirc
#   GitHub     — `gh auth login`

set -euo pipefail

# ── Colours & logging helpers ─────────────────────────────────────────────────

if [ -t 1 ]; then
  RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
  BLUE='\033[0;34m'; DIM='\033[2m'; BOLD='\033[1m'; NC='\033[0m'
else
  RED=''; GREEN=''; YELLOW=''; BLUE=''; DIM=''; BOLD=''; NC=''
fi

step() { echo -e "\n${BLUE}${BOLD}==>${NC}${BOLD} $*${NC}"; }
ok()   { echo -e "  ${GREEN}✓${NC} $*"; }
info() { echo -e "  ${DIM}·${NC} $*"; }
warn() { echo -e "  ${YELLOW}!${NC} $*"; }
run()  { echo -e "  ${DIM}\$ $*${NC}"; "$@"; }
die()  { echo -e "\n${RED}${BOLD}Error:${NC} $*" >&2; exit 1; }

# Ask a yes/no question, reading from the terminal even when stdout is piped.
# Usage: ask "message" [y|n]   → returns 0 for yes, 1 for no.
ask() {
  local msg="$1" default="${2:-n}" prompt reply
  [[ "$default" == "y" ]] && prompt="[Y/n]" || prompt="[y/N]"
  read -r -p "$(echo -e "  ${YELLOW}?${NC} ${msg} ${prompt} ")" reply < /dev/tty
  reply="${reply:-$default}"
  [[ "$reply" =~ ^[Yy]$ ]]
}

# Offer to skip a failed step and continue the release, or abort.
# Usage: confirm_skip "human name of the step"
confirm_skip() {
  local what="$1"
  echo ""
  if ask "'${what}' did not complete. Skip it and continue the release?" n; then
    warn "Skipping '${what}' at your request — continuing."
    return 0
  fi
  die "Stopped at '${what}'. Fix it and re-run; already-completed steps are skipped."
}

# ── Args & validation ─────────────────────────────────────────────────────────

VERSION="${1:-}"
[ -n "$VERSION" ] || die "Usage: $0 vX.Y.Z   (e.g. $0 v0.1.0)"

if [[ ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9._-]+)?$ ]]; then
  die "Version must look like vX.Y.Z (optionally -prerelease); got: '$VERSION'"
fi
BARE="${VERSION#v}"   # crate/npm/PyPI versions are bare semver (no leading 'v')

# Package coordinates, kept in one place.
CRATE_NAME="bae-rs"
NPM_NAME="@prettysmartdev/bae-ts"
PYPI_NAME="bae-py"

echo -e "\n${BOLD}Releasing Better Agent Engine ${VERSION}${NC} ${DIM}(SDK version ${BARE})${NC}"

# ── STEP 1: Pre-flight checks ─────────────────────────────────────────────────

step "Pre-flight checks"

command -v git >/dev/null   || die "git not found on PATH"
command -v curl >/dev/null  || die "curl not found on PATH (needed for registry idempotency checks)"
[ -d .git ] || die "Run this from the repository root (no .git here)."

if command -v gh >/dev/null 2>&1; then
  if gh auth status >/dev/null 2>&1; then
    ok "gh: authenticated"
  else
    warn "gh present but not authenticated (run: gh auth login) — the GitHub-release step will be skippable."
  fi
else
  warn "gh not installed — the GitHub-release step will be skippable (tag + image build still happen)."
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$BRANCH" = "main" ]; then
  ok "Branch: main"
else
  warn "On branch '${BRANCH}', not 'main'."
  ask "Release from '${BRANCH}' anyway?" n || die "Switch to main and re-run."
fi

git fetch origin "$BRANCH" --quiet 2>/dev/null || warn "Could not fetch origin/${BRANCH} (offline?)."
if git rev-parse -q --verify "origin/${BRANCH}" >/dev/null; then
  if [ "$(git rev-parse HEAD)" = "$(git rev-parse "origin/${BRANCH}")" ]; then
    ok "Up to date with origin/${BRANCH}"
  else
    warn "Local ${BRANCH} differs from origin/${BRANCH} (you may be ahead with the release commit, or behind)."
    git status -sb | head -3 | sed 's/^/    /'
    ask "Proceed anyway?" n || die "Sync with origin/${BRANCH} and re-run."
  fi
fi

# Working tree may only carry files this script manages (SDK manifests/locks).
MANAGED_RE='(client-rust/Cargo\.(toml|lock)|client-typescript/(package\.json|package-lock\.json)|client-python/(pyproject\.toml|uv\.lock))'
DIRTY="$(git status --porcelain | grep -Ev "^.. ${MANAGED_RE}$" || true)"
if [ -n "$DIRTY" ]; then
  warn "Working tree has changes outside the release-managed manifests:"
  echo "$DIRTY" | sed 's/^/    /'
  ask "Proceed anyway? (consider committing or stashing these first)" n \
    || die "Commit/stash the above and re-run."
else
  ok "Working tree clean (or only release-managed manifests pending)"
fi

if git ls-remote --tags origin "refs/tags/${VERSION}" 2>/dev/null | grep -q "$VERSION"; then
  warn "Tag ${VERSION} already exists on origin — the push step will no-op, image build already triggered."
else
  ok "Tag ${VERSION} not yet on origin"
fi

# ── STEP 2: Prepare SDK manifests (version + un-mark private) ──────────────────

step "Prepare client SDK manifests"

# Set the `version = "X"` / `"version": "X"` field in a manifest, idempotently.
set_toml_version() {  # file
  local f="$1" cur
  cur="$(grep -m1 '^version' "$f" | sed -E 's/.*=[[:space:]]*"([^"]*)".*/\1/')"
  if [ "$cur" = "$BARE" ]; then info "$(dirname "$f") already at ${BARE}"; return 0; fi
  # Replace only the first `version = "…"` line (the package's own version).
  sed -i.bak "0,/^version = \"${cur}\"/s//version = \"${BARE}\"/" "$f"
  rm -f "$f.bak"
  ok "$(dirname "$f"): ${cur} → ${BARE}"
}
set_json_version() {  # file
  local f="$1" cur
  cur="$(grep -m1 '"version"' "$f" | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/')"
  if [ "$cur" = "$BARE" ]; then info "$(dirname "$f") already at ${BARE}"; return 0; fi
  sed -i.bak -E "s/\"version\"[[:space:]]*:[[:space:]]*\"${cur}\"/\"version\": \"${BARE}\"/" "$f"
  rm -f "$f.bak"
  ok "$(dirname "$f"): ${cur} → ${BARE}"
}

set_toml_version client-rust/Cargo.toml
set_json_version client-typescript/package.json
set_toml_version client-python/pyproject.toml

# Note: this script assumes each manifest is already publishable. The first-release
# "do not publish" markers (Cargo `publish = false`, npm `"private": true`, the PyPI
# `Private :: Do Not Upload` classifier) are removed manually before the first
# release — this script never touches them.

# If the version changed, re-sync lockfiles (best-effort) and commit.
if [ -n "$(git status --porcelain -- client-rust client-typescript client-python)" ]; then
  info "Re-syncing lockfiles to the new version…"
  if command -v cargo >/dev/null; then ( cd client-rust && cargo generate-lockfile --quiet ) || warn "cargo generate-lockfile failed (lock left as-is)"; fi
  if command -v npm   >/dev/null; then ( cd client-typescript && npm install --package-lock-only --ignore-scripts >/dev/null 2>&1 ) || warn "npm lock sync failed (lock left as-is)"; fi
  if command -v uv    >/dev/null; then ( cd client-python && uv lock --quiet ) || warn "uv lock failed (lock left as-is)"; fi

  run git add client-rust/Cargo.toml client-rust/Cargo.lock \
              client-typescript/package.json client-typescript/package-lock.json \
              client-python/pyproject.toml client-python/uv.lock
  run git commit -m "Release ${VERSION}: set client SDK versions to ${BARE}"
  ok "Committed manifest changes for ${VERSION}"
else
  ok "Manifests already prepared for ${VERSION} (nothing to commit)"
fi

# ── STEP 3: Validate locally ──────────────────────────────────────────────────

# Sentinels live inside .git/ so they are never tracked or pushed.
LINT_SENTINEL=".git/.release-lint-passed-${VERSION}"
TEST_SENTINEL=".git/.release-test-passed-${VERSION}"

step "Validate: make lint"
if [ -f "$LINT_SENTINEL" ]; then
  ok "lint already passed for ${VERSION} (sentinel present)"
elif make lint; then
  touch "$LINT_SENTINEL"; ok "make lint passed"
else
  confirm_skip "make lint" && touch "$LINT_SENTINEL"
fi

step "Validate: make test"
if [ -f "$TEST_SENTINEL" ]; then
  ok "tests already passed for ${VERSION} (sentinel present)"
elif make test; then
  touch "$TEST_SENTINEL"; ok "make test passed"
else
  confirm_skip "make test" && touch "$TEST_SENTINEL"
fi

# ── STEP 4: Publish the client SDKs ───────────────────────────────────────────
# Idempotency: each publish first asks the registry whether this exact version is
# already live, and skips if so. Published versions are immutable on all three
# registries, so a re-run never clobbers or double-publishes.

step "Publish ${CRATE_NAME} ${BARE} → crates.io"
# crates.io's API rejects requests without a User-Agent (403), so send one.
if curl -fsS -A "bae-release-script" "https://crates.io/api/v1/crates/${CRATE_NAME}/${BARE}" >/dev/null 2>&1; then
  ok "${CRATE_NAME} ${BARE} already on crates.io — skipping"
elif ! command -v cargo >/dev/null; then
  warn "cargo not found on PATH."; confirm_skip "cargo publish"
else
  if ( cd client-rust && run cargo publish ); then
    ok "Published ${CRATE_NAME} ${BARE} to crates.io"
  else
    confirm_skip "cargo publish"
  fi
fi

step "Publish ${NPM_NAME} ${BARE} → npm"
if command -v npm >/dev/null && npm view "${NPM_NAME}@${BARE}" version >/dev/null 2>&1; then
  ok "${NPM_NAME}@${BARE} already on npm — skipping"
elif ! command -v npm >/dev/null; then
  warn "npm not found on PATH."; confirm_skip "npm publish"
else
  # Build a clean dist/ before publishing (the package ships only dist/).
  if ( cd client-typescript && run npm ci && run npm run build && run npm publish --access public ); then
    ok "Published ${NPM_NAME}@${BARE} to npm"
  else
    confirm_skip "npm publish"
  fi
fi

step "Publish ${PYPI_NAME} ${BARE} → PyPI"
if curl -fsS "https://pypi.org/pypi/${PYPI_NAME}/${BARE}/json" >/dev/null 2>&1; then
  ok "${PYPI_NAME} ${BARE} already on PyPI — skipping"
elif ! command -v uv >/dev/null; then
  warn "uv not found on PATH."; confirm_skip "uv publish"
else
  if ( cd client-python && rm -rf dist && run uv build && run uv publish ); then
    ok "Published ${PYPI_NAME} ${BARE} to PyPI"
  else
    confirm_skip "uv publish"
  fi
fi

# ── STEP 5: Push commit + tag (triggers the GHCR image build) ──────────────────

step "Push release commit + tag ${VERSION}"

if git rev-parse -q --verify "origin/${BRANCH}" >/dev/null \
   && [ "$(git rev-list "origin/${BRANCH}..HEAD" --count 2>/dev/null || echo 0)" -gt 0 ]; then
  run git push origin "$BRANCH"
  ok "Pushed ${BRANCH} to origin"
else
  ok "No new commits to push (${BRANCH} already on origin)"
fi

if git rev-parse -q --verify "refs/tags/${VERSION}" >/dev/null; then
  ok "Tag ${VERSION} already exists locally"
else
  run git tag "$VERSION"
  ok "Created tag ${VERSION}"
fi

if git ls-remote --tags origin "refs/tags/${VERSION}" 2>/dev/null | grep -q "$VERSION"; then
  ok "Tag ${VERSION} already on origin (image build already triggered)"
else
  run git push origin "$VERSION"
  ok "Pushed tag ${VERSION} → GitHub Actions is now building & pushing the GHCR images"
fi

# ── STEP 6: GitHub release ────────────────────────────────────────────────────

step "GitHub release ${VERSION}"
if ! command -v gh >/dev/null 2>&1; then
  warn "gh not installed — create the release manually or install gh, then re-run (idempotent)."
elif ! gh auth status >/dev/null 2>&1; then
  warn "gh not authenticated — run 'gh auth login' then re-run (idempotent)."
elif gh release view "$VERSION" >/dev/null 2>&1; then
  ok "GitHub release ${VERSION} already exists"
else
  if run gh release create "$VERSION" --title "$VERSION" --generate-notes; then
    ok "Created GitHub release ${VERSION}"
  else
    confirm_skip "gh release create"
  fi
fi

# ── Done ──────────────────────────────────────────────────────────────────────

echo ""
echo -e "${GREEN}${BOLD}Release ${VERSION} complete.${NC}"
echo -e "  ${DIM}SDKs:${NC}   ${CRATE_NAME} ${BARE} · ${NPM_NAME}@${BARE} · ${PYPI_NAME} ${BARE}"
echo -e "  ${DIM}Images:${NC} GitHub Actions is publishing"
echo -e "           ghcr.io/prettysmartdev/better-agent-engine:${BARE} (+ :latest)"
echo -e "           ghcr.io/prettysmartdev/better-agent-engine:${BARE}-max (+ :max)"
echo -e "  ${DIM}Watch:${NC}  https://github.com/prettysmartdev/better-agent-engine/actions"
echo ""
