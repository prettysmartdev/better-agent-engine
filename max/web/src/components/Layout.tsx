import type { ReactNode } from "react";

export type TabId = "keys" | "profiles" | "sessions";

const TABS: { id: TabId; label: string }[] = [
  { id: "keys", label: "Keys" },
  { id: "profiles", label: "Profiles" },
  { id: "sessions", label: "Sessions" },
];

/**
 * The dashboard chrome: `max` wordmark top-left and top-bar tabs (deliberately
 * a top bar, not left navigation). Tabs are a real ARIA tablist.
 */
export default function Layout({
  active,
  onSelect,
  onLogout,
  children,
}: {
  active: TabId;
  onSelect: (tab: TabId) => void;
  onLogout: () => void;
  children: ReactNode;
}) {
  return (
    <div className="app">
      <header className="topbar">
        <span className="wordmark">max</span>
        <nav className="tabs" role="tablist" aria-label="Dashboard sections">
          {TABS.map((t) => (
            <button
              key={t.id}
              role="tab"
              id={`tab-${t.id}`}
              aria-selected={active === t.id}
              aria-controls="tabpanel"
              className={active === t.id ? "tab tab-active" : "tab"}
              onClick={() => onSelect(t.id)}
            >
              {t.label}
            </button>
          ))}
        </nav>
        <button className="btn btn-ghost logout" onClick={onLogout}>
          Log out
        </button>
      </header>
      <main
        id="tabpanel"
        role="tabpanel"
        aria-labelledby={`tab-${active}`}
        className="content"
      >
        {children}
      </main>
    </div>
  );
}
