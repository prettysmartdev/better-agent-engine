# CLI Design

Binary name: baesrv
Install path: /usr/local/bin (inside the Docker image; the image is the primary distribution)
Storage location: /var/lib/bae/ (SQLite database and server data; overridable via BAE_DB_PATH)

## Design principles:

### Command structure
Top level command groups:
- `serve` — run the HTTP server (the default when no subcommand is given)
- `migrate` — apply pending database migrations and exit (for operators who want migrations separate from serving)
- `key` — bootstrap/admin key operations (e.g. `key create --role admin`) for recovery without API access
- `version` — print version and supported API versions

### Flag structure
Flag guidance:
- Long flags in kebab-case (`--db-path`, `--addr`); every flag has an environment-variable equivalent (`BAE_DB_PATH`, `BAE_ADDR`) and flags take precedence over env vars.
- No required flags: every option has a sensible default so `baesrv` alone starts a working server.
- `--help` on every command; global `--json` for machine-readable output.

### Inputs and outputs
I/O Guidance:
- stdin: unused; the server is configured entirely via flags/env, not piped input.
- stdout: command results only (e.g. a created key, version info) so output is scriptable; with `--json`, results are single JSON documents.
- stderr: all logs (tracing output), human-readable by default, JSON lines when `BAE_LOG_FORMAT=json`.
- Exit codes: 0 success, 1 runtime error, 2 usage error.

### Configuration
Global config:
- Environment variables are the configuration surface (`BAE_ADDR`, `BAE_DB_PATH`, `BAE_LOG`); no config file is required, matching the Docker-first deployment model.
- If a config file is ever added it will be explicitly opted into via `--config <path>`, with env/flags still taking precedence.
