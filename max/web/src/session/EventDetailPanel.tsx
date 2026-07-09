import type { SessionEvent } from "../api/types";
import { categoryFor } from "../api/eventTypes";
import ShapeMarker from "./ShapeMarker";

function pretty(payload: unknown): string {
  try {
    return JSON.stringify(payload, null, 2);
  } catch {
    return String(payload);
  }
}

/** The detail panel for a selected event node: metadata + pretty-printed payload. */
export default function EventDetailPanel({
  event,
  onClose,
}: {
  event: SessionEvent;
  onClose: () => void;
}) {
  const cat = categoryFor(event.event_type);
  return (
    <aside className="detail-panel" aria-label={`Event ${event.id} detail`}>
      <div className="detail-head">
        <h2 className="detail-title">
          <ShapeMarker category={cat} />
          <code>{event.event_type}</code>
        </h2>
        <button
          className="btn btn-ghost btn-sm"
          onClick={onClose}
          aria-label="Close detail panel"
        >
          ✕
        </button>
      </div>
      <dl className="detail-meta">
        <dt>Event id</dt>
        <dd>{event.id}</dd>
        <dt>Category</dt>
        <dd>{cat.label}</dd>
        <dt>Client key</dt>
        <dd>{event.client_key_id ?? "—"}</dd>
        <dt>Created</dt>
        <dd>{event.created_at}</dd>
      </dl>
      <h3 className="detail-subtitle">Payload</h3>
      <pre className="payload" data-testid="event-payload">
        {pretty(event.payload)}
      </pre>
    </aside>
  );
}
