---
name: release-prep
description: Prepare Better Agent Engine (BAE) for a new release. Sets the version across the three client SDK manifests, updates docs to reflect new features, and drafts release notes. Does NOT run `make release` or push any tags.
---

# BAE Release Prep Skill

Use this skill when the user asks to "prepare for release vX.Y.Z", "bump the version", or "get ready to release" BAE.

## What this skill does

1. Determine the new version from the user's request (e.g. `v0.3.0`)
2. Identify what changed since the last release (git log, new work items, new features)
3. Set the version across all three client SDK manifests
4. Update documentation to reflect new features
5. Draft release notes
6. (Optional) draft a blog post

Do **not** run `make release` or push any tags. Stop before the release script.

> After prep, the human runs `make release VERSION=vX.Y.Z` (see `scripts/release.sh`).
> That script is idempotent: it re-verifies the versions you set, re-syncs the
> lockfiles, validates locally (`make lint` + `make test`), publishes the three SDKs
> to crates.io / npm / PyPI with the maintainer's local credentials, and pushes the
> `vX.Y.Z` tag. Pushing that tag triggers `.github/workflows/release.yml`, which builds
> and pushes both the standard and `bae-max` images to GHCR, multi-arch
> (linux/amd64 + linux/arm64). Your job here is everything a script can't do.

---

## Step 1: Gather context

Run these in parallel:

```bash
# What changed since the last tag? (falls back to full history before the first release)
LAST_TAG=$(git describe --tags --abbrev=0 2>/dev/null)
git log ${LAST_TAG:+$LAST_TAG..}HEAD --oneline

# Current versions — these three must always agree.
grep -H '^version' client-rust/Cargo.toml client-python/pyproject.toml
grep -H '"version"' client-typescript/package.json

# What work items were implemented? (higher numbers are more recent)
ls aspec/work-items/ | sort -r | head -10
```

Also read:
- `aspec/work-items/` — the recent, high-numbered ones describe what shipped; each is the source of truth for its feature.
- `README.md` — the value-focused overview; check whether a headline capability needs a mention.
- `docs/README.md` — the docs index; check that every guide/reference page is linked.
- `docs/guides/` and `docs/reference/` — verify new endpoints, env vars, event types, config, and CLI/harness surfaces are documented (they are usually added alongside the code).
- `DEVELOPING.md` — check whether any build/release/component change needs to be reflected.

---

## Step 2: Set the version (three manifests, kept in lockstep)

BAE ships one version per release, stamped identically across all three client SDKs. Set `X.Y.Z` (the bare semver, no leading `v`) in each:

- `client-rust/Cargo.toml` → `version = "X.Y.Z"`
- `client-typescript/package.json` → `"version": "X.Y.Z"`
- `client-python/pyproject.toml` → `version = "X.Y.Z"`

You do **not** need to touch the lockfiles (`Cargo.lock`, `package-lock.json`, `uv.lock`) — `make release` re-syncs them. `make release` also re-sets these versions idempotently, so setting them here just makes the intended version visible in the prep diff the maintainer reviews.

Do **not** remove the "do not publish" markers (`publish = false`, `"private": true`, the `Private :: Do Not Upload` classifier). Those are removed manually, once, before the first release — neither this skill nor the release script touches them.

---

## Step 3: Update README.md and docs

Reflect anything user-facing that landed since the last release:

- **`README.md`** — value-focused. If a headline capability shipped (a new builtin tool, a new execution mode, a new multiplayer or MAX capability), add or extend the relevant section. Keep the tone: what you get / what you can achieve, not implementation detail.
- **`docs/guides/`** — task-oriented walkthroughs. Add or extend a guide for a significant new capability, following the existing guide structure.
- **`docs/reference/`** — precise spec. Confirm new API routes, `BAE_*` env vars, `bae-config.toml` keys, and `event_type` values are documented.
- **`docs/README.md`** — link any new guide/reference page you added.

Keep additions brief and match the surrounding tone.

---

## Step 4: Draft release notes

BAE's GitHub release notes are auto-generated from merged PR titles (`gh release create --generate-notes`, run by `make release`). Draft a short, curated summary to accompany or replace them, grouped as Features / Improvements / Fixes:

```markdown
# Release vX.Y.Z

## Features

- **Feature name**: one or two sentences on what it does and why it matters.
  - Sub-bullet for sub-commands or options if relevant.

## Improvements

- **Improvement name**: what changed and why it's better.

## Fixes

- Fixed <symptom> in <context>.
```

Save it to `docs/releases/vX.Y.Z.md` for the human record (create `docs/releases/` if it doesn't exist yet), and/or hand it to the maintainer to paste into the GitHub release. Keep it factual and brief — no marketing language, no buzzwords. Focus on what a developer building on BAE would care about.

---

## Step 5 (optional): Blog post

Only if the user asks for one. Draft it under `docs/blog/NNNN-slug.md` (create `docs/blog/` and start at `0001` if the dir doesn't exist).

**Style guide:**
- First-person narrative ("I built this…", "I've been wanting…")
- Open with the problem or itch, not the solution
- Explain *why* the feature matters before *what* it does
- Focus on improved workflows, problems solved, and the trust/security benefits BAE's hybrid + sandbox model brings — not internal mechanics
- Show examples with code/shell blocks (use screenshot placeholders for the human to fill in; don't attempt ASCII art)
- No buzzwords ("revolutionary", "game-changing", "seamless", "robust"); no fluff ("In this post I will…", "I'm excited to announce…")
- Include a quick install/run blurb in the first third (the `docker run … ghcr.io/prettysmartdev/better-agent-engine` one-liner)
- End with a pointer to the GitHub repo and prettysmart.dev, not a long call-to-action. Note that feedback, issues, and contributions are welcome.
- Length: 400–600 words. Longer means you're over-explaining.

---

## Checklist

Before finishing, verify:

- [ ] Version set identically in `client-rust/Cargo.toml`, `client-typescript/package.json`, `client-python/pyproject.toml`
- [ ] `README.md` reflects new headline capabilities
- [ ] `docs/guides/` and `docs/reference/` cover new surfaces (check, don't assume)
- [ ] `docs/README.md` links any new page
- [ ] `docs/releases/vX.Y.Z.md` drafted
- [ ] Blog post drafted only if requested
- [ ] "Do not publish" markers left untouched
- [ ] `make release` NOT run and no tag pushed (the maintainer does that separately)
