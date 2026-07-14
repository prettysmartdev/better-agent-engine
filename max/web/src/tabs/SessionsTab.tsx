import { useEffect, useState } from "react";
import { listSessionEvents, listSessions } from "../api/client";
import type { SessionEvent, SessionListItem } from "../api/types";
import { usePagedList } from "../hooks/usePagedList";
import { EmptyState, ErrorBanner, Spinner } from "../components/ui";
import SessionDetail from "../session/SessionDetail";

/** Snapshot metrics computed once per session when the list loads. */
interface SessionStats {
  events: number;
  /** True when the session has more events than the single page we fetched. */
  eventsCapped: boolean;
  clients: number;
  lastActivity: string | null;
}

export default function SessionsTab() {
  // Default filter: open sessions only. Toggle to include closed/error too.
  const [includeAll, setIncludeAll] = useState(false);
  const [open, setOpen] = useState<SessionListItem | null>(null);

  const sessions = usePagedList(
    (c) => listSessions(includeAll ? undefined : "open", c),
    [includeAll],
  );

  const stats = useSessionStats(sessions.items);

  if (open) {
    return <SessionDetail session={open} onBack={() => setOpen(null)} />;
  }

  return (
    <div className="tab sessions-tab">
      <div className="tab-head">
        <h1 className="tab-title">Sessions</h1>
        <label className="toggle">
          <input
            type="checkbox"
            checked={includeAll}
            onChange={(e) => setIncludeAll(e.target.checked)}
          />
          Show closed &amp; error sessions
        </label>
      </div>

      {sessions.loading && <Spinner />}
      {sessions.error && <ErrorBanner message={sessions.error} />}

      {!sessions.loading && !sessions.error && sessions.items.length === 0 && (
        <EmptyState title={includeAll ? "No sessions yet" : "No open sessions"}>
          <p>
            A session is an agent run against a profile. Open a session with a
            client key to see it here; its events stream live into the graph
            view.
          </p>
          {!includeAll && (
            <button
              className="btn btn-ghost"
              onClick={() => setIncludeAll(true)}
            >
              Include closed &amp; error sessions
            </button>
          )}
        </EmptyState>
      )}

      {sessions.items.length > 0 && (
        <ul className="session-list">
          {sessions.items.map((s) => (
            <SessionRow
              key={s.id}
              session={s}
              stats={stats[s.id]}
              onOpen={() => setOpen(s)}
            />
          ))}
        </ul>
      )}
    </div>
  );
}

function SessionRow({
  session: s,
  stats,
  onOpen,
}: {
  session: SessionListItem;
  stats: SessionStats | undefined;
  onOpen: () => void;
}) {
  return (
    <li
      className="session-card"
      role="button"
      tabIndex={0}
      onClick={onOpen}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onOpen();
        }
      }}
    >
      <div className="session-card-main">
        <div className="session-card-headline">
          <code className="session-card-id">{s.id}</code>
          <span className={`badge state-${s.state}`}>{s.state}</span>
        </div>
        <div className="session-card-sub muted">
          {s.profile_id} · client {s.client_version ?? "unknown"}
        </div>
      </div>

      <div className="session-stats">
        <Stat
          label="Events"
          value={
            stats
              ? stats.eventsCapped
                ? `${stats.events}+`
                : stats.events
              : "…"
          }
        />
        <Stat label="Clients" value={stats ? stats.clients : "…"} />
        <Stat
          label="Last activity"
          value={stats ? timeAgo(stats.lastActivity) : "…"}
        />
        <Stat label="Created" value={formatDate(s.created_at)} />
      </div>

      <div className="session-card-action">
        <button
          className="btn btn-ghost btn-sm"
          onClick={(e) => {
            e.stopPropagation();
            onOpen();
          }}
        >
          Open →
        </button>
      </div>
    </li>
  );
}

function Stat({ label, value }: { label: string; value: string | number }) {
  return (
    <div className="stat">
      <span className="stat-value">{value}</span>
      <span className="stat-label">{label}</span>
    </div>
  );
}

/**
 * Fetches a one-page event snapshot for each listed session and derives the
 * per-session metrics shown on the cards. Deliberately a point-in-time read at
 * load — the counts are not live-updated (the graph view is where you watch a
 * session stream).
 */
function useSessionStats(
  items: SessionListItem[],
): Record<string, SessionStats> {
  const [stats, setStats] = useState<Record<string, SessionStats>>({});

  useEffect(() => {
    if (items.length === 0) {
      setStats({});
      return;
    }
    let cancelled = false;
    (async () => {
      const entries = await Promise.all(
        items.map(async (s) => {
          try {
            const page = await listSessionEvents(s.id);
            return [
              s.id,
              computeStats(s, page.items, !!page.next_cursor),
            ] as const;
          } catch {
            return [s.id, null] as const;
          }
        }),
      );
      if (cancelled) return;
      const next: Record<string, SessionStats> = {};
      for (const [id, st] of entries) if (st) next[id] = st;
      setStats(next);
    })();
    return () => {
      cancelled = true;
    };
  }, [items]);

  return stats;
}

function computeStats(
  s: SessionListItem,
  events: SessionEvent[],
  capped: boolean,
): SessionStats {
  // "Connected clients" = distinct clients that attached (open/join) — only
  // meaningful while the session is still open.
  const clients =
    s.state === "open"
      ? new Set(
          events
            .filter(
              (e) =>
                e.event_type === "session.open" ||
                e.event_type === "session.join",
            )
            .map((e) => e.client_key_id)
            .filter((id): id is string => !!id),
        ).size
      : 0;

  const lastEvent = events.reduce<string>(
    (max, e) => (e.created_at > max ? e.created_at : max),
    "",
  );
  const lastActivity = lastEvent || s.closed_at || s.created_at || null;

  return { events: events.length, eventsCapped: capped, clients, lastActivity };
}

/** Compact "time since" label from an ISO timestamp. */
function timeAgo(iso: string | null): string {
  if (!iso) return "—";
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "—";
  const secs = Math.max(0, Math.floor((Date.now() - then) / 1000));
  if (secs < 60) return `${secs}s ago`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  return `${days}d ago`;
}

/** Short, readable created-at label. */
function formatDate(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}
