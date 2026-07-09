import { useCallback, useEffect, useState } from "react";
import Layout, { type TabId } from "./components/Layout";
import Login from "./components/Login";
import { Spinner } from "./components/ui";
import { checkSession, logout, setUnauthorizedHandler } from "./api/client";
import KeysTab from "./tabs/KeysTab";
import ProfilesTab from "./tabs/ProfilesTab";
import SessionsTab from "./tabs/SessionsTab";

type AuthStatus = "loading" | "authed" | "anon";

export default function App() {
  const [auth, setAuth] = useState<AuthStatus>("loading");
  const [tab, setTab] = useState<TabId>("keys");

  // Any 401 from anywhere in the app forces the login view.
  useEffect(() => {
    setUnauthorizedHandler(() => setAuth("anon"));
    return () => setUnauthorizedHandler(null);
  }, []);

  useEffect(() => {
    let cancelled = false;
    checkSession()
      .then((ok) => {
        if (!cancelled) setAuth(ok ? "authed" : "anon");
      })
      .catch(() => {
        if (!cancelled) setAuth("anon");
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const onLogout = useCallback(() => {
    logout().finally(() => setAuth("anon"));
  }, []);

  if (auth === "loading") {
    return (
      <div className="login-screen">
        <Spinner label="Loading…" />
      </div>
    );
  }

  if (auth === "anon") {
    return <Login onSuccess={() => setAuth("authed")} />;
  }

  return (
    <Layout active={tab} onSelect={setTab} onLogout={onLogout}>
      {tab === "keys" && <KeysTab />}
      {tab === "profiles" && <ProfilesTab />}
      {tab === "sessions" && <SessionsTab />}
    </Layout>
  );
}
