"""Client configuration: where the server is and how to authenticate."""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(slots=True)
class Config:
    """Connection settings for a :class:`~bae_py.harness.Harness`.

    Attributes:
        server_url: Base URL of the BAE client surface, e.g.
            ``http://localhost:8080``. A trailing slash is trimmed so callers
            need not care whether they included one.
        client_key: The ``bae_…`` client key used to open sessions. Treated as
            an opaque bearer string — never parsed or length-validated.
        client_version: Optional free-form version string reported to the
            server at session open (recorded on the ``session.open`` event).
    """

    server_url: str
    client_key: str
    client_version: str | None = None

    def __post_init__(self) -> None:
        self.server_url = self.server_url.rstrip("/")

    def url(self, path: str) -> str:
        """Join ``path`` (leading-slash optional) onto the server base URL."""
        return f"{self.server_url}/{path.lstrip('/')}"
