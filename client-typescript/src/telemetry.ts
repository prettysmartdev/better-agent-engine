/**
 * Intentional runtime-dependency exception: @opentelemetry/api is the
 * TypeScript client’s first runtime dependency. It is API-only and no-op
 * unless the host app installs an OpenTelemetry SDK.
 *
 * This module deliberately does not install a tracer provider, propagator, or
 * context manager. Those are owned by the embedding application.
 */
import {
  context,
  propagation,
  SpanKind,
  SpanStatusCode,
  trace,
  type Span,
} from "@opentelemetry/api";

/** The shared tracer scope required by the cross-SDK telemetry contract. */
export const clientTracer = trace.getTracer("bae.client", "0.1.0");

/**
 * The W3C Trace Context wire allowlist (telemetry contract §6): the only
 * headers BAE ever injects. Anything else an ambient propagator emits —
 * baggage most notably, or any header a custom/composite global propagator
 * writes — is dropped so no baggage value (token, tenant id, prompt fragment)
 * leaks onto a BAE request.
 */
const ALLOWED_PROPAGATION_HEADERS = new Set(["traceparent", "tracestate"]);

/** Inject the ambient W3C context, if the host application has configured one. */
export function injectAmbientContext(headers: Record<string, string>): void {
  // Inject into a throwaway carrier, then copy only the allowlisted trace
  // headers onto the real request headers — never the propagator's full output.
  const carrier: Record<string, string> = {};
  propagation.inject(context.active(), carrier);
  for (const key of Object.keys(carrier)) {
    if (ALLOWED_PROPAGATION_HEADERS.has(key.toLowerCase())) {
      headers[key] = carrier[key];
    }
  }
}

/**
 * Mark a span failed with a fixed, category-only status message.
 *
 * The raw error is deliberately **not** exported (no `recordException`, no
 * error text in the status): a transport/hook/handler error can carry
 * provider-forwarded secrets, tokens, or prompt fragments, and the telemetry
 * contract (§4) forbids any untrusted payload text — including error text —
 * anywhere in telemetry. Full error detail still reaches the caller via the
 * re-thrown exception.
 */
export function recordSpanError(span: Span, category: string): void {
  span.setStatus({ code: SpanStatusCode.ERROR, message: category });
}

export { SpanKind };
