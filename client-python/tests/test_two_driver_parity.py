"""Cross-SDK two-driver parity (WI 0005).

Two client keys attach to one session (driver A via ``connect``, driver B via
``join``, same profile), both register as drivers, both send a message. Every
driver observes the SAME ordered broadcast event sequence — including the other
driver's ``session.join`` / ``session.driver.register`` and, in FIFO order, both
turns' messages. The canonical sequence below MUST stay byte-for-byte identical
to the arrays in:

  - client-rust/src/harness.rs             (TWO_DRIVER_PARITY_SEQUENCE)
  - client-typescript/src/harness.test.ts  (TWO_DRIVER_PARITY_SEQUENCE)

All offline: a scripted mock transport, no server and no API keys.
"""

from __future__ import annotations

from typing import Any

from bae_py import (
    Config,
    Harness,
    Hooks,
    SessionEvent,
    SessionJoinPayload,
)
from mock_transport import MockTransport, connect_response, rpc_notification, rpc_terminal

# The canonical live-notification sequence every driver observes.
TWO_DRIVER_PARITY_SEQUENCE = [
    "session.driver.register",  # driver A registered (connect)
    "session.join",  # driver B joined
    "session.driver.register",  # driver B registered (join)
    "client.message.send",  # driver A's message (FIFO first)
    "provider.request",
    "provider.response",
    "server.message.send",
    "client.message.send",  # driver B's message (FIFO second)
    "provider.request",
    "provider.response",
    "server.message.send",
]

DRIVER_A_KEY = "key_driver_a"
DRIVER_B_KEY = "key_driver_b"


def _event(event_type: str, client_key_id: str, payload: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": f"evt_{event_type}_{client_key_id}",
        "session_id": "ses_two_driver",
        "client_key_id": client_key_id,
        "event_type": event_type,
        "payload": payload,
        "created_at": "t",
    }


def _two_driver_scenario_frames() -> list[dict[str, Any]]:
    """One sendMessage reply carrying the full two-driver broadcast as live
    notifications, then a terminal assistant turn. Both drivers' streams deliver
    this identical sequence (cross-visibility)."""
    notifs = [
        rpc_notification(_event("session.driver.register", DRIVER_A_KEY, {})),
        rpc_notification(
            _event(
                "session.join",
                DRIVER_B_KEY,
                {"client_version": "9.9.9", "tools": ["get_current_time"]},
            )
        ),
        rpc_notification(_event("session.driver.register", DRIVER_B_KEY, {})),
        rpc_notification(
            _event("client.message.send", DRIVER_A_KEY, {"role": "user", "content": "from A"})
        ),
        rpc_notification(_event("provider.request", DRIVER_A_KEY, {"attempt": 0})),
        rpc_notification(_event("provider.response", DRIVER_A_KEY, {"ok": True, "status": 200})),
        rpc_notification(
            _event(
                "server.message.send",
                DRIVER_A_KEY,
                {"role": "assistant", "content": [{"type": "text", "text": "reply A"}]},
            )
        ),
        rpc_notification(
            _event("client.message.send", DRIVER_B_KEY, {"role": "user", "content": "from B"})
        ),
        rpc_notification(_event("provider.request", DRIVER_B_KEY, {"attempt": 0})),
        rpc_notification(_event("provider.response", DRIVER_B_KEY, {"ok": True, "status": 200})),
        rpc_notification(
            _event(
                "server.message.send",
                DRIVER_B_KEY,
                {"role": "assistant", "content": [{"type": "text", "text": "reply B"}]},
            )
        ),
    ]
    terminal = rpc_terminal(
        {
            "message": {"role": "assistant", "content": [{"type": "text", "text": "reply B"}]},
            "events": [],
        }
    )
    return [*notifs, terminal]


async def test_connect_and_join_register_drivers_and_observe_identical_fifo_broadcast() -> None:
    observed_a: list[SessionEvent] = []
    observed_b: list[SessionEvent] = []

    # A shared server: connect returns driver A's key, join returns driver B's.
    transport = MockTransport(
        script=[
            connect_response(session_id="ses_two_driver", session_key="bae_ses_a"),
            connect_response(session_id="ses_two_driver", session_key="bae_ses_b"),
            _two_driver_scenario_frames(),
            _two_driver_scenario_frames(),
        ]
    )

    harness_a = Harness(
        Config(server_url="http://test", client_key="bae_client_a", client_version="9.9.9"),
        hooks=Hooks(on_event=lambda e: observed_a.append(e)),
        transport=transport,
    )
    harness_b = Harness(
        Config(server_url="http://test", client_key="bae_client_b", client_version="9.9.9"),
        hooks=Hooks(on_event=lambda e: observed_b.append(e)),
        transport=transport,
    )

    # Driver A connects; driver B joins the same session.
    session_a = await harness_a.connect()
    session_b = await harness_b.join(session_a.session_id)
    assert session_b.session_id == session_a.session_id

    # Both send a message; each observes the full broadcast.
    await session_a.send("from A")
    await session_b.send("from B")

    # Both drivers observe the identical canonical sequence (cross-visibility).
    assert [e.event_type.value for e in observed_a] == TWO_DRIVER_PARITY_SEQUENCE
    assert [e.event_type.value for e in observed_b] == TWO_DRIVER_PARITY_SEQUENCE
    assert [e.event_type for e in observed_a] == [e.event_type for e in observed_b]

    # connect() and join() each issued exactly one session.registerDriver, with
    # the respective session key.
    assert len(transport.register_driver_calls) == 2
    assert transport.register_driver_calls[0].headers["Authorization"] == "Bearer bae_ses_a"
    assert transport.register_driver_calls[1].headers["Authorization"] == "Bearer bae_ses_b"

    # join() hit the /join path authenticated with driver B's client key.
    join_req = next(r for r in transport.requests if r.url.endswith("/join"))
    assert join_req.method == "POST"
    assert join_req.headers["Authorization"] == "Bearer bae_client_b"

    # Cross-visibility of client keys: an observer sees BOTH drivers' events.
    keys = {e.client_key_id for e in observed_a}
    assert DRIVER_A_KEY in keys
    assert DRIVER_B_KEY in keys

    # FIFO ordering: driver A's message turn precedes driver B's.
    sends = [e for e in observed_a if e.event_type.value == "client.message.send"]
    assert len(sends) == 2
    assert sends[0].client_key_id == DRIVER_A_KEY
    assert sends[1].client_key_id == DRIVER_B_KEY

    # The new session.join payload parses to its real shape.
    join = next(e for e in observed_a if e.event_type.value == "session.join")
    join_payload = SessionJoinPayload.from_payload(join.payload)
    assert join_payload.tools == ["get_current_time"]
    assert join_payload.client_version == "9.9.9"
