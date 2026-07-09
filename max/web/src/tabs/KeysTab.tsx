import { useCallback, useMemo, useState, type FormEvent } from "react";
import { createKey, deleteKey, listKeys, listProfiles } from "../api/client";
import { isObserverKey, type KeyCreated } from "../api/types";
import { usePagedList } from "../hooks/usePagedList";
import { EmptyState, ErrorBanner, Spinner } from "../components/ui";

export default function KeysTab() {
  const keys = usePagedList((c) => listKeys(c));
  const profiles = usePagedList((c) => listProfiles(c));

  const [name, setName] = useState("");
  const [profileId, setProfileId] = useState("");
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);
  const [justCreated, setJustCreated] = useState<KeyCreated | null>(null);

  const profileName = useMemo(() => {
    const m = new Map<string, string>();
    for (const p of profiles.items) m.set(p.id, p.name);
    return (id: string) => m.get(id) ?? id;
  }, [profiles.items]);

  const submit = useCallback(
    async (e: FormEvent) => {
      e.preventDefault();
      if (!name || !profileId) return;
      setCreating(true);
      setCreateError(null);
      try {
        const created = await createKey(name, profileId);
        setJustCreated(created);
        setName("");
        setProfileId("");
        keys.reload();
      } catch (err) {
        setCreateError(
          err instanceof Error ? err.message : "Could not create key.",
        );
      } finally {
        setCreating(false);
      }
    },
    [name, profileId, keys],
  );

  const revoke = useCallback(
    async (id: string, keyName: string) => {
      if (!window.confirm(`Revoke key "${keyName}"? This cannot be undone.`))
        return;
      try {
        await deleteKey(id);
        keys.reload();
      } catch (err) {
        window.alert(
          err instanceof Error ? err.message : "Could not revoke key.",
        );
      }
    },
    [keys],
  );

  const noProfiles = !profiles.loading && profiles.items.length === 0;

  return (
    <div className="tab keys-tab">
      <h1 className="tab-title">Keys</h1>

      {justCreated && (
        <OneTimeKey
          created={justCreated}
          onDismiss={() => setJustCreated(null)}
        />
      )}

      <section className="panel">
        <h2 className="panel-title">Create a client key</h2>
        {noProfiles ? (
          <p className="muted">
            No profiles exist yet. Create a profile first — a key must reference
            one.
          </p>
        ) : (
          <form className="create-form" onSubmit={submit}>
            <label className="field">
              <span className="field-label">Name</span>
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="my-agent-key"
                disabled={creating}
              />
            </label>
            <label className="field">
              <span className="field-label">Profile</span>
              <select
                value={profileId}
                onChange={(e) => setProfileId(e.target.value)}
                disabled={creating || profiles.loading}
              >
                <option value="">Select a profile…</option>
                {profiles.items.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
              </select>
            </label>
            <button
              type="submit"
              className="btn btn-primary"
              disabled={creating || !name || !profileId}
            >
              {creating ? "Creating…" : "Create key"}
            </button>
          </form>
        )}
        {createError && <ErrorBanner message={createError} />}
      </section>

      <section className="panel">
        <h2 className="panel-title">Existing keys</h2>
        {keys.loading && <Spinner />}
        {keys.error && <ErrorBanner message={keys.error} />}
        {!keys.loading && !keys.error && keys.items.length === 0 && (
          <EmptyState title="No keys yet">
            <p>
              A client key lets an agent open sessions under a profile. Create
              one above; the plaintext is shown only once.
            </p>
          </EmptyState>
        )}
        {keys.items.length > 0 && (
          <div className="table-scroll">
            <table className="data-table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Prefix</th>
                  <th>Profile</th>
                  <th>Created</th>
                  <th aria-label="actions" />
                </tr>
              </thead>
              <tbody>
                {keys.items.map((k) => (
                  <tr key={k.id}>
                    <td>
                      <span className="key-name">{k.name}</span>
                      {isObserverKey(k.name) && (
                        <span
                          className="badge badge-observer"
                          title="MAX provisions one observer key per profile it watches"
                        >
                          auto-created by MAX
                        </span>
                      )}
                    </td>
                    <td>
                      <code>{k.prefix}</code>
                    </td>
                    <td>{profileName(k.profile_id)}</td>
                    <td className="muted">{k.created_at ?? "—"}</td>
                    <td className="row-actions">
                      <button
                        className="btn btn-danger btn-sm"
                        onClick={() => revoke(k.id, k.name)}
                      >
                        Revoke
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </div>
  );
}

function OneTimeKey({
  created,
  onDismiss,
}: {
  created: KeyCreated;
  onDismiss: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(created.key);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };
  return (
    <div className="panel one-time-key" role="alert">
      <h2 className="panel-title">Key created — copy it now</h2>
      <p className="warning-text">
        <strong>This is the only time the plaintext key is shown.</strong> It
        cannot be retrieved again. Copy and store it securely before dismissing.
      </p>
      <div className="key-reveal">
        <code className="plaintext-key" data-testid="plaintext-key">
          {created.key}
        </code>
        <button className="btn btn-primary btn-sm" onClick={copy}>
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <button className="btn btn-ghost btn-sm" onClick={onDismiss}>
        I've saved it — dismiss
      </button>
    </div>
  );
}
