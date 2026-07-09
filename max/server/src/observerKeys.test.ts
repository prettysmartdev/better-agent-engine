import { describe, expect, it, vi } from "vitest";
import {
  ObserverKeyProvisioner,
  type KeyFileStore,
  type ObserverKeyFile,
} from "./observerKeys.js";
import type { AdminClient } from "./adminClient.js";

/** An in-memory KeyFileStore standing in for the 0600 JSON file. */
function memStore(initial: ObserverKeyFile = {}): KeyFileStore & {
  data: ObserverKeyFile;
  writes: number;
} {
  const state = {
    data: { ...initial },
    writes: 0,
    read(): ObserverKeyFile {
      return { ...state.data };
    },
    write(contents: ObserverKeyFile): void {
      state.data = { ...contents };
      state.writes += 1;
    },
  };
  return state;
}

function adminWith(createKey: ReturnType<typeof vi.fn>): AdminClient {
  return { createKey } as unknown as AdminClient;
}

describe("ObserverKeyProvisioner", () => {
  it("mints and persists a max-observer-<profile> key on first observe", async () => {
    const store = memStore();
    const createKey = vi.fn(async () => ({
      id: "key_abc",
      name: "max-observer-pro_1",
      key: "bae_secret",
      profile_id: "pro_1",
    }));
    const prov = new ObserverKeyProvisioner(adminWith(createKey), store);

    const entry = await prov.observerKey("pro_1");

    expect(entry).toEqual({ key: "bae_secret", key_id: "key_abc" });
    expect(createKey).toHaveBeenCalledExactlyOnceWith({
      name: "max-observer-pro_1",
      profile_id: "pro_1",
    });
    expect(store.data.pro_1).toEqual({ key: "bae_secret", key_id: "key_abc" });
  });

  it("reuses the persisted key on the second observe (no admin call)", async () => {
    const store = memStore({
      pro_1: { key: "bae_cached", key_id: "key_cached" },
    });
    const createKey = vi.fn();
    const prov = new ObserverKeyProvisioner(adminWith(createKey), store);

    const entry = await prov.observerKey("pro_1");

    expect(entry).toEqual({ key: "bae_cached", key_id: "key_cached" });
    expect(createKey).not.toHaveBeenCalled();
    expect(store.writes).toBe(0);
  });

  it("mints only once under concurrent observes for the same profile", async () => {
    const store = memStore();
    const createKey = vi.fn(async () => ({
      id: "key_x",
      key: "bae_x",
    }));
    const prov = new ObserverKeyProvisioner(adminWith(createKey), store);

    const [a, b] = await Promise.all([
      prov.observerKey("pro_1"),
      prov.observerKey("pro_1"),
    ]);

    expect(a).toEqual(b);
    expect(createKey).toHaveBeenCalledTimes(1);
  });

  it("does not clobber other profiles' entries when persisting a new one", async () => {
    const store = memStore({
      pro_other: { key: "bae_other", key_id: "key_other" },
    });
    const createKey = vi.fn(async () => ({ id: "key_1", key: "bae_1" }));
    const prov = new ObserverKeyProvisioner(adminWith(createKey), store);

    await prov.observerKey("pro_1");

    expect(store.data).toEqual({
      pro_other: { key: "bae_other", key_id: "key_other" },
      pro_1: { key: "bae_1", key_id: "key_1" },
    });
  });

  it("retries cleanly after a failed mint (rejection not cached)", async () => {
    const store = memStore();
    const createKey = vi
      .fn()
      .mockRejectedValueOnce(new Error("boom"))
      .mockResolvedValueOnce({ id: "key_ok", key: "bae_ok" });
    const prov = new ObserverKeyProvisioner(adminWith(createKey), store);

    await expect(prov.observerKey("pro_1")).rejects.toThrow("boom");
    const entry = await prov.observerKey("pro_1");
    expect(entry).toEqual({ key: "bae_ok", key_id: "key_ok" });
    expect(createKey).toHaveBeenCalledTimes(2);
  });
});
