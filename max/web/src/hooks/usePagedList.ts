import { useCallback, useEffect, useState } from "react";
import type { Page } from "../api/types";
import { UnauthorizedError } from "../api/client";

export interface PagedListState<T> {
  items: T[];
  loading: boolean;
  error: string | null;
  reload: () => void;
}

/**
 * Loads every page of a cursor-paginated resource into one array. `fetchPage`
 * takes a cursor (undefined for the first page) and returns a `Page<T>`.
 * `deps` re-triggers the load (e.g. a filter change).
 */
export function usePagedList<T>(
  fetchPage: (cursor?: string) => Promise<Page<T>>,
  deps: unknown[] = [],
): PagedListState<T> {
  const [items, setItems] = useState<T[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);

  const reload = useCallback(() => setNonce((n) => n + 1), []);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);

    (async () => {
      try {
        const acc: T[] = [];
        let cursor: string | undefined;
        // Bound the page walk so a misbehaving cursor can't loop forever.
        for (let i = 0; i < 1000; i++) {
          const page = await fetchPage(cursor);
          acc.push(...page.items);
          if (!page.next_cursor) break;
          cursor = page.next_cursor;
        }
        if (!cancelled) setItems(acc);
      } catch (e) {
        if (cancelled) return;
        if (e instanceof UnauthorizedError) return; // handled globally → login
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();

    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [nonce, ...deps]);

  return { items, loading, error, reload };
}
