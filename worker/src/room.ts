// One room: its small persistent state in SQLite and the WebSockets of the
// participants connected to it. Sockets use the hibernation API, so an idle
// room is evicted from memory without dropping its connections (and without
// counting duration on the Free plan); anything a socket must remember goes
// in its attachment, never in instance fields.

import { DurableObject } from "cloudflare:workers";

/** Largest message accepted, the same as the app's signaling limit. */
export const MAX_MESSAGE_BYTES = 64 * 1024;
export const SCHEMA_VERSION = 1;
/** Keepalive answered by the runtime without waking the room. */
export const KEEPALIVE = { request: "ping", response: "pong" } as const;

/** What a socket remembers across hibernation. */
export interface Attachment {
  connectedAt: number;
}

export class Room extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.migrate();
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair(KEEPALIVE.request, KEEPALIVE.response));
  }

  /** Creates or upgrades the tables; runs on every start, so it must be
   * idempotent. Storage calls are synchronous, so no request can run before
   * it finishes. */
  private migrate(): void {
    const sql = this.ctx.storage.sql;
    sql.exec("CREATE TABLE IF NOT EXISTS schema (version INTEGER NOT NULL)");
    const current = sql.exec<{ version: number }>("SELECT version FROM schema").toArray()[0];
    if (current === undefined) {
      sql.exec("INSERT INTO schema (version) VALUES (?)", SCHEMA_VERSION);
    }
    // Later versions: `if (current.version < 2) { ...; UPDATE schema ... }`.
  }

  schemaVersion(): number {
    return this.ctx.storage.sql.exec<{ version: number }>("SELECT version FROM schema").one().version;
  }

  async fetch(_request: Request): Promise<Response> {
    const { 0: client, 1: server } = new WebSocketPair();
    this.ctx.acceptWebSocket(server);
    const attachment: Attachment = { connectedAt: Date.now() };
    server.serializeAttachment(attachment);
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    if (typeof message !== "string") {
      ws.close(1003, "text messages only");
      return;
    }
    if (new TextEncoder().encode(message).byteLength > MAX_MESSAGE_BYTES) {
      ws.close(1009, "message too big");
      return;
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(message);
    } catch {
      send(ws, { type: "error", reason: "not JSON" });
      return;
    }
    if (isObject(parsed) && parsed.type === "hello") {
      send(ws, {
        type: "welcome",
        schema: this.schemaVersion(),
        connections: this.ctx.getWebSockets().length,
      });
      return;
    }
    send(ws, { type: "error", reason: "unknown message" });
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string): Promise<void> {
    // Complete the closing handshake (without it the client sees 1006). Codes
    // 1005 and 1006 only describe a close and cannot be sent back.
    try {
      ws.close(code === 1005 || code === 1006 ? 1000 : code, reason);
    } catch {
      // Already closed: nothing to answer.
    }
  }
}

function send(ws: WebSocket, message: object): void {
  ws.send(JSON.stringify(message));
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
