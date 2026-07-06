"""Security primitives, per the work item: randomness comes from
``secrets.token_bytes`` and constant-time comparison from ``hmac.compare_digest``.

The harness itself treats keys as opaque and does no crypto, but consumers who
mint correlation tags or verify their own tokens should reach for these rather
than ``random``/``==``.
"""

from __future__ import annotations

import hmac
import secrets


def random_hex(n_bytes: int) -> str:
    """Return ``n_bytes`` of cryptographically-secure randomness as lowercase hex.

    Backed by :func:`secrets.token_bytes` — never ``random``, which is not
    suitable for anything security-sensitive.
    """
    if n_bytes < 0:
        raise ValueError("n_bytes must be non-negative")
    return secrets.token_bytes(n_bytes).hex()


def constant_time_equal(a: str | bytes, b: str | bytes) -> bool:
    """Compare two secrets without leaking their relationship through timing.

    Wraps :func:`hmac.compare_digest`. Both arguments must be the same type
    (both ``str`` or both ``bytes``); ``str`` inputs must be ASCII.
    """
    return hmac.compare_digest(a, b)
