"""Client-side tool definitions: what the harness declares at session open and
dispatches ``tool_use`` blocks to.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, Union

from .types import Content

# A handler receives the tool's ``input`` object and returns the result
# *content* (a string or a list of content blocks). It may be sync or async;
# the harness awaits the result if it is awaitable.
ToolHandler = Callable[[dict[str, Any]], Union[Content, Awaitable[Content]]]


@dataclass(slots=True)
class Tool:
    """A tool the harness can execute on the client's behalf.

    ``name``/``description``/``input_schema`` are declared to the server at
    session open (and validated against the profile's ``allowed_tools``). When
    the model calls the tool, ``handler`` is invoked with the call's ``input``
    and its return value is wrapped into a ``tool_result`` block echoing the
    originating ``tool_use.id``.
    """

    name: str
    description: str
    input_schema: dict[str, Any]
    handler: ToolHandler

    def declaration(self) -> dict[str, Any]:
        """The ``{name, description, input_schema}`` sent at session open."""
        return {
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
        }


@dataclass(slots=True)
class ToolRegistry:
    """Name → :class:`Tool` lookup used by the harness loop."""

    _tools: dict[str, Tool] = field(default_factory=dict)

    def add(self, tool: Tool) -> None:
        self._tools[tool.name] = tool

    def get(self, name: str) -> Tool | None:
        return self._tools.get(name)

    def declarations(self) -> list[dict[str, Any]]:
        return [t.declaration() for t in self._tools.values()]

    def __len__(self) -> int:
        return len(self._tools)

    def __contains__(self, name: object) -> bool:
        return name in self._tools
