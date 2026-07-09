//! Per-profile observer client keys, lazily provisioned and persisted
//! (work item 0007, section C).
//!
//! `join` enforces `client_key.profile_id == session.profile_id`, so MAX cannot
//! reuse one key across profiles — it needs one client key per profile. On the
//! first observe for a profile, MAX mints a `max-observer-<profile_id>` client
//! key via the admin API and persists the one-time plaintext to a `0600` file
//! on the shared data volume. Every later observe for that profile reads the
//! file and never re-mints.

import fs from "node:fs";

import type { AdminClient } from "./adminClient.js";

/** One persisted observer key entry. */
export interface ObserverKeyEntry {
  /** The `bae_...` client-key plaintext (shown once at creation). */
  key: string;
  /** The `key_...` id, so an operator can correlate/revoke it. */
  key_id: string;
}

/** On-disk file shape: `{ "<profile_id>": { key, key_id }, ... }`. */
export type ObserverKeyFile = Record<string, ObserverKeyEntry>;

/** Injectable filesystem seam so provisioning is unit-testable without disk. */
export interface KeyFileStore {
  read(): ObserverKeyFile;
  write(contents: ObserverKeyFile): void;
}

/** Default `KeyFileStore` backed by a `0600` JSON file at `path`. */
export function fileKeyStore(path: string): KeyFileStore {
  return {
    read(): ObserverKeyFile {
      try {
        const raw = fs.readFileSync(path, "utf8");
        const parsed = JSON.parse(raw) as unknown;
        if (parsed && typeof parsed === "object") {
          return parsed as ObserverKeyFile;
        }
        return {};
      } catch (err) {
        if ((err as NodeJS.ErrnoException).code === "ENOENT") return {};
        throw err;
      }
    },
    write(contents: ObserverKeyFile): void {
      fs.writeFileSync(path, `${JSON.stringify(contents, null, 2)}\n`, {
        mode: 0o600,
      });
      fs.chmodSync(path, 0o600);
    },
  };
}

/**
 * Provisions and caches per-profile observer client keys.
 *
 * `observerKey(profileId)` is idempotent and concurrency-safe: the first call
 * for a profile mints and persists a key; overlapping and subsequent calls
 * return the same entry without a second admin-API round-trip (an in-flight
 * mint is memoized by its promise).
 */
export class ObserverKeyProvisioner {
  private readonly inflight = new Map<string, Promise<ObserverKeyEntry>>();

  constructor(
    private readonly admin: AdminClient,
    private readonly store: KeyFileStore,
  ) {}

  observerKey(profileId: string): Promise<ObserverKeyEntry> {
    const existing = this.inflight.get(profileId);
    if (existing) return existing;

    const promise = this.provision(profileId).catch((err) => {
      // A failed mint must not be cached as a permanent rejection — drop it so
      // the next observe retries cleanly.
      this.inflight.delete(profileId);
      throw err;
    });
    this.inflight.set(profileId, promise);
    return promise;
  }

  private async provision(profileId: string): Promise<ObserverKeyEntry> {
    const file = this.store.read();
    const cached = file[profileId];
    if (cached && cached.key && cached.key_id) {
      return cached;
    }

    const created = (await this.admin.createKey({
      name: `max-observer-${profileId}`,
      profile_id: profileId,
    })) as { key?: string; id?: string };

    if (!created.key || !created.id) {
      throw new Error(
        `admin POST /keys for profile ${profileId} returned no key/id`,
      );
    }
    const entry: ObserverKeyEntry = { key: created.key, key_id: created.id };

    // Re-read before writing so a concurrent writer's entries aren't clobbered.
    const latest = this.store.read();
    latest[profileId] = entry;
    this.store.write(latest);
    return entry;
  }
}
