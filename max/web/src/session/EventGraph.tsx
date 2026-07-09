import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import type { SessionEvent } from "../api/types";
import { categoryFor } from "../api/eventTypes";
import ShapeMarker from "./ShapeMarker";

const ROW_HEIGHT = 64;
const OVERSCAN = 6;

/**
 * A chronological event graph rendered as a virtualized vertical timeline —
 * only the nodes near the viewport are mounted, so thousands of events never
 * mount at once. This list-first layout doubles as the mobile/tablet fallback
 * (it pans by ordinary scrolling; no wide desktop canvas is assumed).
 *
 * Each node is keyboard-focusable; Enter/Space opens the same detail panel a
 * click does.
 */
export default function EventGraph({
  events,
  selectedId,
  onSelect,
}: {
  events: SessionEvent[];
  selectedId: number | null;
  onSelect: (event: SessionEvent) => void;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  // Fallback height keeps virtualization sane before layout is measured
  // (and in jsdom, where clientHeight is 0).
  const [viewport, setViewport] = useState(600);
  const followRef = useRef(true);

  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const measure = () => {
      if (el.clientHeight > 0) setViewport(el.clientHeight);
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, []);

  // Live-tail: if the user is parked at the bottom, keep following new events.
  useEffect(() => {
    const el = scrollRef.current;
    if (el && followRef.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [events.length]);

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    setScrollTop(el.scrollTop);
    followRef.current =
      el.scrollTop + el.clientHeight >= el.scrollHeight - ROW_HEIGHT;
  }, []);

  const total = events.length;
  const start = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  const end = Math.min(
    total,
    Math.ceil((scrollTop + viewport) / ROW_HEIGHT) + OVERSCAN,
  );
  const visible = events.slice(start, end);

  return (
    <div
      className="graph-scroll"
      ref={scrollRef}
      onScroll={onScroll}
      role="list"
      aria-label="Session events"
      tabIndex={0}
    >
      <div className="graph-canvas" style={{ height: total * ROW_HEIGHT }}>
        {visible.map((ev, i) => {
          const index = start + i;
          return (
            <GraphNode
              key={ev.id}
              event={ev}
              top={index * ROW_HEIGHT}
              selected={ev.id === selectedId}
              onSelect={onSelect}
            />
          );
        })}
      </div>
    </div>
  );
}

function GraphNode({
  event,
  top,
  selected,
  onSelect,
}: {
  event: SessionEvent;
  top: number;
  selected: boolean;
  onSelect: (event: SessionEvent) => void;
}) {
  const cat = categoryFor(event.event_type);
  const activate = () => onSelect(event);
  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" || e.key === " " || e.key === "Spacebar") {
      e.preventDefault();
      activate();
    }
  };
  return (
    <div
      role="listitem"
      className="graph-node-row"
      style={{ top, height: ROW_HEIGHT }}
    >
      <div
        role="button"
        tabIndex={0}
        aria-pressed={selected}
        aria-label={`${event.event_type} event ${event.id}`}
        className={selected ? "graph-node graph-node-selected" : "graph-node"}
        onClick={activate}
        onKeyDown={onKeyDown}
      >
        <ShapeMarker category={cat} />
        <span className="node-body">
          <span className="node-type">{event.event_type}</span>
          <span className="node-meta">
            #{event.id}
            {event.client_key_id ? ` · ${event.client_key_id}` : ""} ·{" "}
            {event.created_at}
          </span>
        </span>
      </div>
    </div>
  );
}
