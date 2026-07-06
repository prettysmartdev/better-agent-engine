# User Interface

(use cli.md if solely designing a CLI-based UX)

BAE currently has no graphical interface: the product surface is the REST
API, the `baesrv` CLI (see uxui/cli.md), and the three client
libraries. This file records the constraints an eventual admin/debug web
dashboard must follow, and the machine-use guidance that applies today.

## Style

Aesthetic:
- Developer-tool minimalism: dense, legible, monospace-friendly; content (sessions, events, runs) over chrome.

Brand and colors:
- Neutral dark/light palette with a single accent color; no branding decisions locked in yet.

Desktop vs mobile:
- Desktop-first (it's an operator/developer tool); responsive enough to check a run from a phone, but no mobile-specific features.

## Usage

Layout:
- If/when a dashboard exists: left navigation of resource types (agents, sessions, runs, keys), main pane of lists and detail views mirroring the API objects one-to-one — the UI should teach the API, not hide it.

Menus:
- Shallow: no more than two levels; every screen reachable within two clicks and addressable by URL.

Empty states:
- Every empty list explains what the resource is and shows the API/client-library call that would create one.

Accessibility:
- Keyboard-navigable, semantic HTML, WCAG AA contrast from the start; no interaction that requires a pointer.

Machine use:
- The API is the machine interface: everything a future UI can do must be possible via `/api/v1` (the UI is a pure API client).
- Publish an OpenAPI document for the API so tooling and agents can consume the surface programmatically.
