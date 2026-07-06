/** Connection configuration for a {@link Harness}. */
export interface ConfigOptions {
  /** Base URL of the BAE client surface, e.g. `http://localhost:8080`. */
  serverUrl: string;
  /** The client key (`bae_…`) exchanged for a session on `connect()`. */
  clientKey: string;
  /** Optional client version string, recorded on the session. */
  clientVersion?: string;
}

/**
 * Immutable connection configuration: the server URL, the client key, and an
 * optional client version. Trailing slashes on the URL are normalized away.
 */
export class Config {
  readonly serverUrl: string;
  readonly clientKey: string;
  readonly clientVersion?: string;

  constructor(options: ConfigOptions) {
    this.serverUrl = options.serverUrl.replace(/\/+$/, "");
    this.clientKey = options.clientKey;
    this.clientVersion = options.clientVersion;
  }
}
