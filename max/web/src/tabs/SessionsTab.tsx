import { useState } from "react";
import { listSessions } from "../api/client";
import type { SessionListItem } from "../api/types";
import { usePagedList } from "../hooks/usePagedList";
import { EmptyState, ErrorBanner, Spinner } from "../components/ui";
import SessionDetail from "../session/SessionDetail";

export default function SessionsTab() {
  // Default filter: open sessions only. Toggle to include closed/error too.
  const [includeAll, setIncludeAll] = useState(false);
  const [open, setOpen] = useState<SessionListItem | null>(null);

  const sessions = usePagedList(
    (c) => listSessions(includeAll ? undefined : "open", c),
    [includeAll],
  );

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
        <div className="table-scroll">
          <table className="data-table">
            <thead>
              <tr>
                <th>Session</th>
                <th>Profile</th>
                <th>State</th>
                <th>Client</th>
                <th>Created</th>
                <th aria-label="actions" />
              </tr>
            </thead>
            <tbody>
              {sessions.items.map((s) => (
                <tr
                  key={s.id}
                  className="clickable-row"
                  onClick={() => setOpen(s)}
                >
                  <td>
                    <code>{s.id}</code>
                  </td>
                  <td>{s.profile_id}</td>
                  <td>
                    <span className={`badge state-${s.state}`}>{s.state}</span>
                  </td>
                  <td className="muted">{s.client_version ?? "—"}</td>
                  <td className="muted">{s.created_at}</td>
                  <td className="row-actions">
                    <button
                      className="btn btn-ghost btn-sm"
                      onClick={(e) => {
                        e.stopPropagation();
                        setOpen(s);
                      }}
                    >
                      Open
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
