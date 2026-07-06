"""Tests for the security primitives (secrets-backed randomness, constant-time
comparison)."""

from __future__ import annotations

import pytest

from bae_py import constant_time_equal, random_hex


def test_random_hex_length_and_alphabet() -> None:
    token = random_hex(16)
    assert len(token) == 32  # two hex chars per byte
    assert all(c in "0123456789abcdef" for c in token)


def test_random_hex_is_unpredictable() -> None:
    assert random_hex(16) != random_hex(16)


def test_random_hex_zero_bytes() -> None:
    assert random_hex(0) == ""


def test_random_hex_rejects_negative() -> None:
    with pytest.raises(ValueError):
        random_hex(-1)


def test_constant_time_equal_str() -> None:
    assert constant_time_equal("bae_abc", "bae_abc") is True
    assert constant_time_equal("bae_abc", "bae_xyz") is False


def test_constant_time_equal_bytes() -> None:
    assert constant_time_equal(b"\x01\x02", b"\x01\x02") is True
    assert constant_time_equal(b"\x01\x02", b"\x01\x03") is False
