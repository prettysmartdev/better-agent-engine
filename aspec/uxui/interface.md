# User Interface

(use cli.md if solely designing a CLI-based UX)

BAE's product surface is the REST API, the `baesrv` CLI (see uxui/cli.md),
the three client libraries, and — as of work item 0007 — **MAX**, a browser
dashboard shipped in the `bae-max` container image variant (`max/`, see
architecture/design.md's Component 6 and
[docs/guides/max-webapp.md](../../docs/guides/max-webapp.md)). MAX is the
admin/debug web dashboard this file anticipated; the sections below describe
what it actually is rather than what a hypothetical future one might be.

## Style

Aesthetic:
- Developer-tool minimalism: dense, legible, monospace-friendly; content (sessions, events, runs) over chrome.

Brand and colors:
- Neutral dark/light palette with a single accent color; no branding decisions locked in yet.

Desktop vs mobile:
- Mobile and tablet friendly, not just "responsive enough to check from a phone" — every MAX view (Keys, Profiles, Sessions, and the event graph specifically) is fully usable on a phone or tablet browser, not a shrunk-down desktop layout. The event graph in particular falls back to pan/zoom or a list-first view on narrow viewports rather than assuming a wide canvas, and paginates/virtualizes large sessions so it stays usable on tighter mobile memory/CPU budgets.

## Usage

Layout:
- MAX's shipped layout: a `max` wordmark top-left, and top-bar tabs — `Keys` / `Profiles` / `Sessions` — not left navigation. This intentionally departs from an earlier left-navigation sketch: top-bar tabs read better across the full mobile-to-desktop width range this dashboard has to support, where a persistent side rail competes with content on narrow viewports. Each tab's main pane is a list and detail view mirroring the API objects one-to-one — the UI still teaches the API rather than hiding it, that part of the original intent is unchanged.

Menus:
- Shallow: no more than two levels; every screen reachable within two clicks and addressable by URL.

Empty states:
- Every empty list explains what the resource is and shows the API/client-library call that would create one.

Accessibility:
- Keyboard-navigable, semantic HTML, WCAG AA contrast from the start; no interaction that requires a pointer.

Machine use:
- The API is the machine interface: everything a future UI can do must be possible via `/api/v1` (the UI is a pure API client).
- Publish an OpenAPI document for the API so tooling and agents can consume the surface programmatically.
