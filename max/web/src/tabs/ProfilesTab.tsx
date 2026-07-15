import { useCallback, useState, type FormEvent } from "react";
import {
  createProfile,
  deleteProfile,
  listMcpServers,
  listProfiles,
  listProviders,
  updateProfile,
} from "../api/client";
import type { Profile, ProfileInput } from "../api/types";
import { usePagedList } from "../hooks/usePagedList";
import { EmptyState, ErrorBanner, Spinner } from "../components/ui";

type Editing = { mode: "create" } | { mode: "edit"; profile: Profile } | null;

export default function ProfilesTab() {
  const profiles = usePagedList((c) => listProfiles(c));
  const providers = usePagedList(() => listProviders());
  const mcpServers = usePagedList(() => listMcpServers());

  const [editing, setEditing] = useState<Editing>(null);

  const noProviders = !providers.loading && providers.items.length === 0;

  const remove = useCallback(
    async (p: Profile) => {
      if (!window.confirm(`Delete profile "${p.name}"?`)) return;
      try {
        await deleteProfile(p.id);
        profiles.reload();
      } catch (err) {
        window.alert(
          err instanceof Error ? err.message : "Could not delete profile.",
        );
      }
    },
    [profiles],
  );

  if (editing) {
    return (
      <ProfileForm
        editing={editing}
        providerNames={providers.items.map((p) => p.name)}
        mcpNames={mcpServers.items.map((m) => m.name)}
        onDone={() => {
          setEditing(null);
          profiles.reload();
        }}
        onCancel={() => setEditing(null)}
      />
    );
  }

  return (
    <div className="profiles-tab">
      <div className="tab-head">
        <h1 className="tab-title">Profiles</h1>
        <button
          className="btn btn-primary"
          disabled={noProviders}
          title={
            noProviders
              ? "Configure a provider before creating a profile"
              : undefined
          }
          onClick={() => setEditing({ mode: "create" })}
        >
          New profile
        </button>
      </div>

      {noProviders && (
        <div className="banner banner-warn" role="status">
          No providers are configured, so a profile could never resolve one.
          Profile creation is disabled until at least one provider exists.
        </div>
      )}

      {profiles.loading && <Spinner />}
      {profiles.error && <ErrorBanner message={profiles.error} />}

      {!profiles.loading && !profiles.error && profiles.items.length === 0 && (
        <EmptyState title="No profiles yet">
          <p>
            A <strong>profile</strong> bundles a primary LLM provider, optional
            fallbacks, MCP servers, and allowed tools. Client keys reference a
            profile to decide how their sessions run.
          </p>
          {!noProviders && (
            <button
              className="btn btn-primary"
              onClick={() => setEditing({ mode: "create" })}
            >
              Create your first profile
            </button>
          )}
        </EmptyState>
      )}

      {profiles.items.length > 0 && (
        <div className="table-scroll">
          <table className="data-table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Primary provider</th>
                <th>Fallbacks</th>
                <th>MCP servers</th>
                <th aria-label="actions" />
              </tr>
            </thead>
            <tbody>
              {profiles.items.map((p) => (
                <tr key={p.id}>
                  <td>{p.name}</td>
                  <td>{p.primary_provider || "—"}</td>
                  <td className="muted">
                    {p.fallback_providers.join(", ") || "—"}
                  </td>
                  <td className="muted">{p.mcp_servers.join(", ") || "—"}</td>
                  <td className="row-actions">
                    <button
                      className="btn btn-ghost btn-sm"
                      onClick={() => setEditing({ mode: "edit", profile: p })}
                    >
                      Edit
                    </button>
                    <button
                      className="btn btn-danger btn-sm"
                      onClick={() => remove(p)}
                    >
                      Delete
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

function splitTools(raw: string): string[] {
  return raw
    .split(/[\n,]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

function ProfileForm({
  editing,
  providerNames,
  mcpNames,
  onDone,
  onCancel,
}: {
  editing: Exclude<Editing, null>;
  providerNames: string[];
  mcpNames: string[];
  onDone: () => void;
  onCancel: () => void;
}) {
  const existing = editing.mode === "edit" ? editing.profile : null;
  const [name, setName] = useState(existing?.name ?? "");
  const [primary, setPrimary] = useState(existing?.primary_provider ?? "");
  const [fallbacks, setFallbacks] = useState<string[]>(
    existing?.fallback_providers ?? [],
  );
  const [mcp, setMcp] = useState<string[]>(existing?.mcp_servers ?? []);
  const [toolsRaw, setToolsRaw] = useState(
    (existing?.allowed_tools ?? []).join("\n"),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const toggle = (
    list: string[],
    set: (v: string[]) => void,
    value: string,
  ) => {
    set(
      list.includes(value) ? list.filter((v) => v !== value) : [...list, value],
    );
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!name || !primary) return;
    const input: ProfileInput = {
      name,
      primary_provider: primary,
      fallback_providers: fallbacks,
      mcp_servers: mcp,
      allowed_tools: splitTools(toolsRaw),
    };
    setBusy(true);
    setError(null);
    try {
      if (existing) await updateProfile(existing.id, input);
      else await createProfile(input);
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not save profile.");
      setBusy(false);
    }
  };

  return (
    <div className="tab">
      <div className="tab-head">
        <h1 className="tab-title">
          {existing ? `Edit ${existing.name}` : "New profile"}
        </h1>
      </div>
      <form className="panel profile-form" onSubmit={submit}>
        <label className="field">
          <span className="field-label">Name</span>
          <input
            value={name}
            onChange={(e) => setName(e.target.value)}
            disabled={busy}
          />
        </label>

        <label className="field">
          <span className="field-label">Primary provider</span>
          <select
            value={primary}
            onChange={(e) => setPrimary(e.target.value)}
            disabled={busy}
          >
            <option value="">Select a provider…</option>
            {providerNames.map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </label>

        <fieldset className="field checkbox-group">
          <legend className="field-label">Fallback providers</legend>
          {providerNames.length === 0 ? (
            <p className="muted">No providers available.</p>
          ) : (
            providerNames
              .filter((n) => n !== primary)
              .map((n) => (
                <label key={n} className="checkbox-row">
                  <input
                    type="checkbox"
                    checked={fallbacks.includes(n)}
                    onChange={() => toggle(fallbacks, setFallbacks, n)}
                    disabled={busy}
                  />
                  {n}
                </label>
              ))
          )}
        </fieldset>

        <fieldset className="field checkbox-group">
          <legend className="field-label">MCP servers</legend>
          {mcpNames.length === 0 ? (
            <p className="muted">No MCP servers configured.</p>
          ) : (
            mcpNames.map((n) => (
              <label key={n} className="checkbox-row">
                <input
                  type="checkbox"
                  checked={mcp.includes(n)}
                  onChange={() => toggle(mcp, setMcp, n)}
                  disabled={busy}
                />
                {n}
              </label>
            ))
          )}
        </fieldset>

        <label className="field">
          <span className="field-label">Allowed tools</span>
          <textarea
            value={toolsRaw}
            onChange={(e) => setToolsRaw(e.target.value)}
            rows={4}
            placeholder="One tool name per line (client-declared; free text)"
            disabled={busy}
          />
          <span className="field-hint">
            Free text — tool names are client-declared, so these are not
            pickers.
          </span>
        </label>

        {error && <ErrorBanner message={error} />}

        <div className="form-actions">
          <button
            type="submit"
            className="btn btn-primary"
            disabled={busy || !name || !primary}
          >
            {busy ? "Saving…" : existing ? "Save changes" : "Create profile"}
          </button>
          <button
            type="button"
            className="btn btn-ghost"
            onClick={onCancel}
            disabled={busy}
          >
            Cancel
          </button>
        </div>
      </form>
    </div>
  );
}
