import { useMemo, useState } from "react";
import type { SessionEvent, SessionListItem } from "../api/types";
import { EVENT_CATEGORIES } from "../api/eventTypes";
import { useSessionStream, type ConnectFn } from "./useSessionStream";
import EventGraph from "./EventGraph";
import EventDetailPanel from "./EventDetailPanel";
import ShapeMarker from "./ShapeMarker";
import { ErrorBanner, Spinner } from "../components/ui";

/**
 * The event-graph detail view for one session: full history + live tail over
 * the observer WebSocket, a virtualized node timeline, and a payload panel.
 */
export default function SessionDetail({
  session,
  onBack,
  connect,
}: {
  session: SessionListItem;
  onBack: () => void;
  connect?: ConnectFn;
}) {
  const { events, status, endedReason, errorMessage } = useSessionStream(
    session.id,
    session.profile_id,
    session.state,
    connect,
  );
  const [selectedId, setSelectedId] = useState<number | null>(null);

  const selected = useMemo(
    () => events.find((e) => e.id === selectedId) ?? null,
    [events, selectedId],
  );

  return (
    <div className={`session-detail ${selected ? "has-panel" : ""}`}>
      <div className="tab-head detail-toolbar">
        <button className="btn btn-ghost" onClick={onBack}>
          ← Sessions
        </button>
        <div className="detail-heading">
          <code className="session-id">{session.id}</code>
          <StatusBadge status={status} endedReason={endedReason} />
        </div>
      </div>

      {status === "error" && (
        <ErrorBanner message={errorMessage ?? "Stream error."} />
      )}

      <div className="graph-legend" aria-label="Event categories">
        {EVENT_CATEGORIES.map((c) => (
          <span key={c.key} className="legend-item">
            <ShapeMarker category={c} />
            {c.label}
          </span>
        ))}
      </div>

      <div className="graph-layout">
        <div className="graph-main">
          {status === "connecting" && events.length === 0 ? (
            <Spinner label="Loading history…" />
          ) : events.length === 0 ? (
            <p className="muted graph-empty">
              No events recorded for this session.
            </p>
          ) : (
            <EventGraph
              events={events}
              selectedId={selectedId}
              onSelect={(e) => setSelectedId(e.id)}
            />
          )}
        </div>
        {selected && (
          <SelectedPanel event={selected} onClose={() => setSelectedId(null)} />
        )}
      </div>
    </div>
  );
}

function SelectedPanel({
  event,
  onClose,
}: {
  event: SessionEvent;
  onClose: () => void;
}) {
  return (
    <div className="panel-slot">
      <EventDetailPanel event={event} onClose={onClose} />
    </div>
  );
}

function StatusBadge({
  status,
  endedReason,
}: {
  status: string;
  endedReason?: "closed" | "error";
}) {
  if (status === "streaming") {
    return (
      <span className="status-badge status-live" data-testid="stream-status">
        <span className="live-dot" aria-hidden="true" /> live
      </span>
    );
  }
  if (status === "connecting") {
    return (
      <span className="status-badge" data-testid="stream-status">
        connecting…
      </span>
    );
  }
  if (status === "error") {
    return (
      <span className="status-badge status-ended" data-testid="stream-status">
        stream error
      </span>
    );
  }
  // ended
  return (
    <span className="status-badge status-ended" data-testid="stream-status">
      session ended{endedReason === "error" ? " (error)" : ""}
    </span>
  );
}
