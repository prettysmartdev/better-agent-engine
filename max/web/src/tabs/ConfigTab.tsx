import { useEffect, useState } from "react";
import { getConfig } from "../api/client";
import type {
  ConfigResponse,
  McpServerConfigView,
  ProviderConfigView,
  TelemetryConfigView,
} from "../api/types";
import { ErrorBanner, Spinner } from "../components/ui";

/** Read-only view of the bae-config.toml snapshot retained by the server. */
export default function ConfigTab() {
  const [config, setConfig] = useState<ConfigResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    getConfig()
      .then((response) => {
        if (!cancelled) setConfig(response);
      })
      .catch((err) => {
        if (!cancelled)
          setError(
            err instanceof Error ? err.message : "Could not load config.",
          );
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (error) return <ErrorBanner message={error} />;
  if (!config) return <Spinner label="Loading configuration…" />;

  return (
    <div className="config-tab">
      <h1 className="tab-title">Config</h1>
      <McpSection servers={config.mcp.servers} />
      <ProvidersSection entries={config.providers.entries} />
      <TelemetrySection telemetry={config.telemetry} />
    </div>
  );
}

function SectionHeading({ id, children }: { id: string; children: string }) {
  return (
    <div className="tab-head">
      <div>
        <h2 id={id} className="panel-title">
          {children}
        </h2>
        <p className="muted">as loaded at server startup</p>
      </div>
    </div>
  );
}

function McpSection({ servers }: { servers: McpServerConfigView[] }) {
  return (
    <section aria-labelledby="mcp-servers-heading">
      <SectionHeading id="mcp-servers-heading">MCP Servers</SectionHeading>
      {servers.length === 0 ? (
        <div className="panel">
          <p className="muted">
            No MCP servers are configured in <code>bae-config.toml</code>.
          </p>
        </div>
      ) : (
        servers.map((server) => (
          <McpServerCard key={server.name} server={server} />
        ))
      )}
    </section>
  );
}

function McpServerCard({ server }: { server: McpServerConfigView }) {
  const command = [server.command, ...server.args].filter(
    (value): value is string => value !== null,
  );

  return (
    <article className="panel">
      <h3 className="panel-title">
        {server.name} <span className="badge">{server.transport}</span>
      </h3>
      <div className="table-scroll">
        <table className="data-table">
          <tbody>
            {server.transport === "stdio" ? (
              <tr>
                <th scope="row">Command</th>
                <td>
                  <code>{command.join(" ") || "—"}</code>
                </td>
              </tr>
            ) : (
              <tr>
                <th scope="row">URL</th>
                <td>{server.url ?? "—"}</td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <HeaderList
        title="Headers"
        headers={server.headers}
        empty="No headers configured"
      />
    </article>
  );
}

function ProvidersSection({ entries }: { entries: ProviderConfigView[] }) {
  return (
    <section aria-labelledby="providers-heading">
      <SectionHeading id="providers-heading">Providers</SectionHeading>
      {entries.length === 0 ? (
        <div className="panel">
          <p className="muted">
            No providers are configured in <code>bae-config.toml</code>.
          </p>
        </div>
      ) : (
        entries.map((entry) => <ProviderCard key={entry.name} entry={entry} />)
      )}
    </section>
  );
}

function ProviderCard({ entry }: { entry: ProviderConfigView }) {
  return (
    <article className="panel">
      <h3 className="panel-title">{entry.name}</h3>
      <div className="table-scroll">
        <table className="data-table">
          <tbody>
            <tr>
              <th scope="row">Provider</th>
              <td>{entry.provider}</td>
            </tr>
            <tr>
              <th scope="row">Model</th>
              <td>{entry.model}</td>
            </tr>
            <tr>
              <th scope="row">Base URL</th>
              <td>{entry.base_url}</td>
            </tr>
            <tr>
              <th scope="row">Auth token</th>
              <td>
                <code>{entry.auth_token}</code>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </article>
  );
}

function TelemetrySection({ telemetry }: { telemetry: TelemetryConfigView }) {
  return (
    <section aria-labelledby="telemetry-heading">
      <SectionHeading id="telemetry-heading">Telemetry</SectionHeading>
      <article className="panel">
        <h3 className="panel-title">
          Enabled:{" "}
          {telemetry.enabled ? <span className="badge">yes</span> : "no"}
        </h3>
        {!telemetry.enabled ? (
          <p className="muted">OpenTelemetry export is disabled</p>
        ) : (
          <>
            <div className="table-scroll">
              <table className="data-table">
                <tbody>
                  <tr>
                    <th scope="row">OTLP endpoint</th>
                    <td>{telemetry.otlp_endpoint ?? "—"}</td>
                  </tr>
                  <tr>
                    <th scope="row">Sample ratio</th>
                    <td>{telemetry.sample_ratio}</td>
                  </tr>
                  <tr>
                    <th scope="row">Service name</th>
                    <td>{telemetry.service_name}</td>
                  </tr>
                  <tr>
                    <th scope="row">Traces enabled</th>
                    <td>{telemetry.traces.enabled ? "yes" : "no"}</td>
                  </tr>
                  <tr>
                    <th scope="row">Metrics enabled</th>
                    <td>{telemetry.metrics.enabled ? "yes" : "no"}</td>
                  </tr>
                  <tr>
                    <th scope="row">Disabled metrics</th>
                    <td>
                      {telemetry.metrics.disabled.length === 0 ? (
                        "none"
                      ) : (
                        <ul>
                          {telemetry.metrics.disabled.map((metric) => (
                            <li key={metric}>{metric}</li>
                          ))}
                        </ul>
                      )}
                    </td>
                  </tr>
                </tbody>
              </table>
            </div>
            <HeaderList
              title="OTLP headers"
              headers={telemetry.otlp_headers}
              empty="No OTLP headers configured"
            />
          </>
        )}
      </article>
    </section>
  );
}

function HeaderList({
  title,
  headers,
  empty,
}: {
  title: string;
  headers: Record<string, string>;
  empty: string;
}) {
  const entries = Object.entries(headers);
  return (
    <div>
      <h4 className="panel-title">{title}</h4>
      {entries.length === 0 ? (
        <p className="muted">{empty}</p>
      ) : (
        <div className="table-scroll">
          <table className="data-table">
            <tbody>
              {entries.map(([name, value]) => (
                <tr key={name}>
                  <th scope="row">{name}</th>
                  <td>
                    <code>{value}</code>
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
