"""Optional lifecycle hooks. Each hook receives the relevant event object, may
mutate it in place or log it, and may raise to abort the loop (surfaced as a
:class:`~bae_py.errors.HookError`).
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Awaitable, Callable, Optional, Union

from .types import Message, SessionEvent, ToolResultBlock, ToolUseBlock

# A hook is any callable taking the event; return value is ignored. Sync or
# async — the harness awaits it if it is awaitable.
Hook = Callable[[Any], Union[None, Awaitable[None]]]


@dataclass(slots=True)
class Hooks:
    """The harness hook points (all optional).

    * ``before_send`` — an outgoing :class:`Message` about to be sent
      (fires for every turn, including the tool-result turn).
    * ``after_receive`` — the assistant :class:`Message` just received.
    * ``before_tool_call`` — a :class:`ToolUseBlock` about to be dispatched.
    * ``after_tool_call`` — the :class:`ToolResultBlock` produced by a handler,
      before it is sent back (rewriting ``content`` changes what is sent).
    * ``on_event`` — a live :class:`SessionEvent` observed on the ``/rpc``
      notification stream during a turn, in arrival order (read-only; the same
      events are also available in bulk from :attr:`Session.last_events`).
    """

    before_send: Optional[Callable[[Message], Union[None, Awaitable[None]]]] = None
    after_receive: Optional[Callable[[Message], Union[None, Awaitable[None]]]] = None
    before_tool_call: Optional[Callable[[ToolUseBlock], Union[None, Awaitable[None]]]] = None
    after_tool_call: Optional[Callable[[ToolResultBlock], Union[None, Awaitable[None]]]] = None
    on_event: Optional[Callable[[SessionEvent], Union[None, Awaitable[None]]]] = None
