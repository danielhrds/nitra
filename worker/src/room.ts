// One room: its participants and invites in SQLite, and the WebSockets of
// the participants connected now. Sockets use the hibernation API, so an
// idle room leaves memory without dropping its connections (and without
// counting duration on the Free plan); what a socket must remember lives in
// its attachment, never in instance fields.
//
// Entering a room (no passwords, no accounts):
// 1. On connect the room sends a fresh random challenge for this socket.
// 2. The app signs `authMessage(room, challenge)` with its device key and
//    sends `auth` with its public key, the signature and its name, plus
//    `create` (a new room) or an invite token (a new participant).
// 3. The room verifies the signature; a participant enters, a valid invite
//    makes a new participant (one use), `create` makes the first
//    participant the owner. Anything else closes the socket.
//
// Roles: the owner revokes invites and removes participants; any participant
// creates invites. When the owner leaves for good, the oldest participant
// becomes the owner. When the last participant leaves, the room's data is
// deleted.

import { DurableObject } from "cloudflare:workers";

import { authMessage, base32Encode, hexDecode, hexEncode, randomBytes, sha256Hex, verifyEd25519 } from "./crypto";
import { type ClientMessage, parse } from "./protocol";

/** Largest message accepted, the same as the app's signaling limit. */
export const MAX_MESSAGE_BYTES = 64 * 1024;
export const SCHEMA_VERSION = 2;
/** Keepalive answered by the runtime without waking the room. */
export const KEEPALIVE = { request: "ping", response: "pong" } as const;
/** A socket that does not authenticate in this time is closed. */
export const AUTH_TIMEOUT_MS = 20_000;
/** Open (unexpired, unused, unrevoked) invites a room may hold. */
export const MAX_OPEN_INVITES = 100;
/** The Worker passes the room id to the room in this header. */
export const ROOM_HEADER = "X-Ziz-Room";
export const CHALLENGE_BYTES = 32;
/** `WebSocket.readyState` of an open socket. */
const OPEN = 1;

/** Close codes (4000-4999 are for applications). */
export const CLOSE = {
  authFailed: 4001,
  notAllowed: 4003,
  removed: 4004,
  authTimeout: 4008,
} as const;

/** What a socket remembers across hibernation. */
export interface Attachment {
  connectedAt: number;
  room: string;
  /** Hex of this socket's one-time challenge. */
  challenge: string;
  /** The participant's key code once authenticated. */
  key?: string;
}

export type Role = "owner" | "member";

type MemberRow = {
  key: string;
  name: string;
  role: Role;
  joined_at: number;
};

type InviteRow = {
  id: string;
  created_by: string;
  created_at: number;
  expires_at: number;
  uses_left: number;
};

export class Room extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair(KEEPALIVE.request, KEEPALIVE.response));
  }

  private get sql(): SqlStorage {
    return this.ctx.storage.sql;
  }

  /** Whether the room has participants. Reads only: probing an unknown
   * room id must not create storage. */
  private exists(): boolean {
    const table = this.sql.exec("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'members'").toArray();
    return table.length > 0 && this.sql.exec("SELECT 1 FROM members LIMIT 1").toArray().length > 0;
  }

  /** Creates or upgrades the tables (idempotent). Runs only when a room is
   * created, so ids nobody created leave no data behind. */
  private migrate(): void {
    this.sql.exec("CREATE TABLE IF NOT EXISTS schema (version INTEGER NOT NULL)");
    let version = this.sql.exec<{ version: number }>("SELECT version FROM schema").toArray()[0]?.version;
    if (version === undefined) {
      this.sql.exec("INSERT INTO schema (version) VALUES (1)");
      version = 1;
    }
    if (version < 2) {
      this.sql.exec(`CREATE TABLE members (
        key TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        role TEXT NOT NULL CHECK (role IN ('owner', 'member')),
        joined_at INTEGER NOT NULL,
        invited_by TEXT
      )`);
      this.sql.exec(`CREATE TABLE invites (
        id TEXT PRIMARY KEY,
        hash TEXT NOT NULL UNIQUE,
        created_by TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        uses_left INTEGER NOT NULL,
        revoked INTEGER NOT NULL DEFAULT 0
      )`);
      this.sql.exec("UPDATE schema SET version = 2");
    }
  }

  schemaVersion(): number | undefined {
    if (this.sql.exec("SELECT 1 FROM sqlite_master WHERE name = 'schema'").toArray().length === 0) {
      return undefined;
    }
    return this.sql.exec<{ version: number }>("SELECT version FROM schema").one().version;
  }

  async fetch(request: Request): Promise<Response> {
    const { 0: client, 1: server } = new WebSocketPair();
    this.ctx.acceptWebSocket(server);
    const challenge = randomBytes(CHALLENGE_BYTES);
    const attachment: Attachment = {
      connectedAt: Date.now(),
      room: request.headers.get(ROOM_HEADER) ?? "",
      challenge: hexEncode(challenge),
    };
    server.serializeAttachment(attachment);
    send(server, { type: "challenge", challenge: attachment.challenge });
    if ((await this.ctx.storage.getAlarm()) === null) {
      await this.ctx.storage.setAlarm(attachment.connectedAt + AUTH_TIMEOUT_MS);
    }
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
    const attachment = ws.deserializeAttachment() as Attachment;
    const parsed = parse(message);
    if (attachment.key === undefined) {
      if ("error" in parsed || parsed.type !== "auth") {
        ws.close(CLOSE.authFailed, "error" in parsed ? parsed.error : "authenticate first");
        return;
      }
      await this.authenticate(ws, attachment, parsed);
      return;
    }
    if ("error" in parsed) {
      send(ws, { type: "error", reason: parsed.error });
      return;
    }
    const me = this.member(attachment.key);
    if (!me) {
      // Removed or left from another socket meanwhile.
      ws.close(CLOSE.removed, "no longer a participant");
      return;
    }
    await this.handle(ws, me, parsed);
  }

  private async authenticate(
    ws: WebSocket,
    attachment: Attachment,
    auth: Extract<ClientMessage, { type: "auth" }>,
  ): Promise<void> {
    const challenge = hexDecode(attachment.challenge) ?? new Uint8Array();
    const signed = await verifyEd25519(auth.key, authMessage(attachment.room, challenge), auth.signature);
    if (!signed) {
      ws.close(CLOSE.authFailed, "bad signature");
      return;
    }
    const inviteHash = auth.invite ? await sha256Hex(auth.invite) : null;
    // From here on no awaits: checking and changing membership happens in
    // one synchronous step, so two sockets cannot both take an invite's last
    // use or both create the room.
    const now = Date.now();
    let joined = false;
    if (auth.create) {
      if (this.exists()) {
        ws.close(CLOSE.notAllowed, "the room already exists");
        return;
      }
      this.migrate();
      this.sql.exec(
        "INSERT INTO members (key, name, role, joined_at) VALUES (?, ?, 'owner', ?)",
        auth.keyCode,
        auth.name,
        now,
      );
      joined = true;
    } else if (this.exists() && this.member(auth.keyCode)) {
      this.sql.exec("UPDATE members SET name = ? WHERE key = ?", auth.name, auth.keyCode);
    } else {
      const invite = inviteHash && this.exists() ? this.takeInvite(inviteHash, now) : null;
      if (!invite) {
        ws.close(CLOSE.notAllowed, auth.invite ? "invalid invite" : "not a participant");
        return;
      }
      this.sql.exec(
        "INSERT INTO members (key, name, role, joined_at, invited_by) VALUES (?, ?, 'member', ?, ?)",
        auth.keyCode,
        auth.name,
        now,
        invite.created_by,
      );
      joined = true;
    }
    attachment.key = auth.keyCode;
    ws.serializeAttachment(attachment);
    const me = this.member(auth.keyCode) as MemberRow;
    send(ws, { type: "welcome", you: publicMember(me), members: this.members().map(publicMember) });
    if (joined) {
      this.broadcast({ type: "member.joined", member: publicMember(me) }, ws);
    }
  }

  /** Uses one use of the open invite with this token hash, if any. */
  private takeInvite(hash: string, now: number): InviteRow | null {
    const invite = this.sql
      .exec<InviteRow>(
        `SELECT id, created_by, created_at, expires_at, uses_left FROM invites
         WHERE hash = ? AND revoked = 0 AND expires_at > ? AND uses_left > 0`,
        hash,
        now,
      )
      .toArray()[0];
    if (!invite) {
      return null;
    }
    this.sql.exec("UPDATE invites SET uses_left = uses_left - 1 WHERE id = ?", invite.id);
    return invite;
  }

  private async handle(ws: WebSocket, me: MemberRow, message: ClientMessage): Promise<void> {
    const now = Date.now();
    switch (message.type) {
      case "auth":
        send(ws, { type: "error", reason: "already authenticated" });
        return;
      case "invite.create": {
        const token = base32Encode(randomBytes(16));
        const hash = await sha256Hex(token);
        // No awaits from here: the count and the insert are one step.
        this.dropClosedInvites(now);
        const open = this.sql.exec<{ n: number }>("SELECT count(*) AS n FROM invites").one().n;
        if (open >= MAX_OPEN_INVITES) {
          send(ws, { type: "error", reason: "too many open invites" });
          return;
        }
        const id = hexEncode(randomBytes(8));
        const expiresAt = now + message.ttlSecs * 1000;
        this.sql.exec(
          `INSERT INTO invites (id, hash, created_by, created_at, expires_at, uses_left)
           VALUES (?, ?, ?, ?, ?, ?)`,
          id,
          hash,
          me.key,
          now,
          expiresAt,
          message.maxUses,
        );
        send(ws, { type: "invite.created", id, token, expires_at: expiresAt, uses_left: message.maxUses });
        return;
      }
      case "invite.list": {
        this.dropClosedInvites(now);
        const invites = this.sql
          .exec<InviteRow>("SELECT id, created_by, created_at, expires_at, uses_left FROM invites ORDER BY created_at")
          .toArray();
        send(ws, { type: "invites", invites });
        return;
      }
      case "invite.revoke": {
        if (me.role !== "owner") {
          send(ws, { type: "error", reason: "only the owner can revoke invites" });
          return;
        }
        const changed = this.sql.exec("UPDATE invites SET revoked = 1 WHERE id = ? AND revoked = 0", message.id)
          .rowsWritten;
        send(ws, changed > 0 ? { type: "invite.revoked", id: message.id } : { type: "error", reason: "unknown invite" });
        return;
      }
      case "member.remove": {
        if (me.role !== "owner") {
          send(ws, { type: "error", reason: "only the owner can remove participants" });
          return;
        }
        if (message.keyCode === me.key) {
          send(ws, { type: "error", reason: "the owner leaves with room.leave" });
          return;
        }
        if (!this.member(message.keyCode)) {
          send(ws, { type: "error", reason: "not a participant" });
          return;
        }
        this.sql.exec("DELETE FROM members WHERE key = ?", message.keyCode);
        this.closeSocketsOf(message.keyCode, CLOSE.removed, "removed from the room");
        this.broadcast({ type: "member.removed", key: message.keyCode, by: me.key });
        return;
      }
      case "room.leave":
        await this.leave(me);
        return;
    }
  }

  /** `me` leaves for good: hand the room to the oldest participant if `me`
   * owned it, or delete the room if nobody is left. */
  private async leave(me: MemberRow): Promise<void> {
    this.sql.exec("DELETE FROM members WHERE key = ?", me.key);
    this.closeSocketsOf(me.key, 1000, "left the room");
    const next = this.members()[0];
    if (!next) {
      // Nobody left: delete everything. Sockets that never authenticated
      // stay and may create the room again.
      await this.ctx.storage.deleteAlarm();
      await this.ctx.storage.deleteAll();
      if (this.unauthenticated().length > 0) {
        await this.ctx.storage.setAlarm(Date.now() + AUTH_TIMEOUT_MS);
      }
      return;
    }
    this.broadcast({ type: "member.left", key: me.key });
    if (me.role === "owner") {
      this.sql.exec("UPDATE members SET role = 'owner' WHERE key = ?", next.key);
      this.broadcast({ type: "owner.changed", key: next.key });
    }
  }

  /** Closes sockets that did not authenticate in time. */
  async alarm(): Promise<void> {
    const now = Date.now();
    let next: number | undefined;
    for (const ws of this.unauthenticated()) {
      const deadline = (ws.deserializeAttachment() as Attachment).connectedAt + AUTH_TIMEOUT_MS;
      if (deadline <= now) {
        ws.close(CLOSE.authTimeout, "authentication timeout");
      } else {
        next = Math.min(next ?? deadline, deadline);
      }
    }
    if (next !== undefined) {
      await this.ctx.storage.setAlarm(next);
    }
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

  private dropClosedInvites(now: number): void {
    this.sql.exec("DELETE FROM invites WHERE revoked = 1 OR uses_left <= 0 OR expires_at <= ?", now);
  }

  private member(key: string): MemberRow | undefined {
    return this.sql
      .exec<MemberRow>("SELECT key, name, role, joined_at FROM members WHERE key = ?", key)
      .toArray()[0];
  }

  /** Participants, oldest first. */
  private members(): MemberRow[] {
    return this.sql
      .exec<MemberRow>("SELECT key, name, role, joined_at FROM members ORDER BY joined_at, key")
      .toArray();
  }

  private unauthenticated(): WebSocket[] {
    return this.ctx.getWebSockets().filter((ws) => (ws.deserializeAttachment() as Attachment).key === undefined);
  }

  private closeSocketsOf(key: string, code: number, reason: string): void {
    for (const ws of this.ctx.getWebSockets()) {
      if (ws.readyState === OPEN && (ws.deserializeAttachment() as Attachment).key === key) {
        ws.close(code, reason);
      }
    }
  }

  /** Sends to every open authenticated socket except `except` (sockets
   * closed a moment ago are still listed by the runtime). */
  private broadcast(message: object, except?: WebSocket): void {
    for (const ws of this.ctx.getWebSockets()) {
      if (ws !== except && ws.readyState === OPEN && (ws.deserializeAttachment() as Attachment).key !== undefined) {
        send(ws, message);
      }
    }
  }
}

function publicMember(m: MemberRow): object {
  return { key: m.key, name: m.name, role: m.role, joined_at: m.joined_at };
}

function send(ws: WebSocket, message: object): void {
  ws.send(JSON.stringify(message));
}
