# Agents

BAE is an engine for hosting agents, not a collection of agents: end-user
agents are defined by agent developers through the client harnesses, and the
engine is provider-agnostic (provider credentials come from the environment;
model and provider are per-agent configuration, never hardcoded in the
engine). The agents below are the ones this repository itself ships, as
examples and test fixtures.

## Agent 1:
Name: reference-assistant
Purpose: Canonical example agent, implemented once per client library, demonstrating the harness end to end (define agent → open session → run loop → tools).
Model: claude (a current Sonnet-class model by default; configurable per deployment)
Provider: anthropic (default; any supported provider can be substituted in config)
Description:
- A small task assistant shipped in each client's examples/ directory with identical behavior across Rust, TypeScript, and Python — it doubles as the parity check between the three harnesses.
Guidance:
- Keep it minimal and readable: it is documentation first, product second. Every harness customization point should be exercised at least once across the example.
- Provider credentials come from the developer's environment (e.g. ANTHROPIC_API_KEY); the example must fail with a clear message when they are missing.

## Agent 2:
Name: harness-smoke
Purpose: Deterministic agent used by integration tests to exercise the server and client harnesses without any LLM provider.
Model: other (none — a scripted mock provider with canned responses)
Provider: other (in-process mock)
Description:
- Drives the full agent/session/event/run lifecycle against a real server with a throwaway SQLite database, asserting identical observable behavior from all three clients.
Guidance:
- Never calls a real provider: tests must run offline, in CI, with no secrets.
- Keep its canned scripts alongside the integration tests and versioned with the API surface, so wire-contract changes show up as test diffs.
