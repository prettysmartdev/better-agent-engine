# Experience

BAE is self-hosted developer infrastructure: there is no hosted signup, web
account, or billing. "Accounts" are API keys managed by the operator of each
server instance.

## Signup and account

Signup flow:
- None (self-hosted). Getting access = an operator creates an `agent` API key for you and hands it over out of band (see uxui/setup.md).

Account management:
- API keys are the account unit: created, listed (by hash prefix and label), and revoked via admin endpoints; keys carry a human-readable label and role.
- Revocation is immediate; there is no self-service password/credential reset — lost keys are revoked and reissued by an operator.

Invitations and team/group management:
- Not in scope for now: one server instance ≈ one team. Multi-tenancy, if ever added, would arrive as key-scoped namespaces rather than user/group objects.

RBAC/permissions:
- Two roles, `admin` and `agent`, enforced per key (see architecture/security.md). Resources are attributed to the creating key; `agent` keys see only their own resources.

Billing, subscriptions, plans:
- None. The project is open source (Apache-2.0); LLM provider costs are borne directly by whoever supplies the provider API keys.

## Regular usage

Login flow:
- No interactive login: clients send their bearer key on every request. Client libraries read the server URL and key from constructor arguments or the `BAE_URL`/`BAE_API_KEY` environment variables.

Emails, notifications, texts:
- None sent by the platform. Anything user-facing (an agent notifying a human) is agent behavior implemented by the agent developer, not a BAE feature; run status is available by polling or SSE streaming on the API.
