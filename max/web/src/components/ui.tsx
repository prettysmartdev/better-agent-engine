// Small presentational primitives shared across tabs.

import type { ReactNode } from "react";

/**
 * The `BAE` / `MAX` wordmark: two monospace rows so the six letters line up
 * as a 3×2 grid. `BAE` is neon-blue (the readme logo palette); `MAX` is white,
 * on a dark chip that keeps both legible in either theme.
 */
export function Wordmark({ className }: { className?: string }) {
  return (
    <span
      className={className ? `wordmark ${className}` : "wordmark"}
      role="img"
      aria-label="BAE MAX"
    >
      <span className="wordmark-bae" aria-hidden="true">
        BAE
      </span>
      <span className="wordmark-max" aria-hidden="true">
        MAX
      </span>
    </span>
  );
}

export function Spinner({ label = "Loading…" }: { label?: string }) {
  return (
    <div className="spinner" role="status" aria-live="polite">
      <span className="spinner-dot" aria-hidden="true" />
      {label}
    </div>
  );
}

export function ErrorBanner({ message }: { message: string }) {
  return (
    <div className="banner banner-error" role="alert">
      {message}
    </div>
  );
}

export function EmptyState({
  title,
  children,
}: {
  title: string;
  children?: ReactNode;
}) {
  return (
    <div className="empty-state">
      <h2>{title}</h2>
      {children}
    </div>
  );
}
