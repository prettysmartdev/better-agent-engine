//! A tiny incremental NDJSON line splitter, used to parse baesrv's
//! `POST /api/v1/sessions/{id}/rpc` streaming response. Pure and synchronous so
//! the framing logic is unit-testable independent of any real socket.

/** Splits a growing byte/text stream into complete JSON lines. */
export class NdjsonBuffer {
  private buf = "";

  /**
   * Append a chunk and return every complete line it completed. A trailing
   * partial line is retained internally until its terminating newline arrives.
   */
  push(chunk: string): string[] {
    this.buf += chunk;
    const lines: string[] = [];
    let nl = this.buf.indexOf("\n");
    while (nl !== -1) {
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (line.trim() !== "") lines.push(line);
      nl = this.buf.indexOf("\n");
    }
    return lines;
  }

  /** Any buffered, not-yet-newline-terminated remainder (usually empty). */
  flush(): string | undefined {
    const rest = this.buf.trim();
    this.buf = "";
    return rest === "" ? undefined : rest;
  }
}
