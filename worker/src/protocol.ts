// The messages participants send to a room, validated into typed values.
// Anything malformed becomes an error string: the room never acts on
// unchecked input.

import { base32Decode, base32Encode, hexDecode } from "./crypto";

export const PUBLIC_KEY_BYTES = 32;
export const SIGNATURE_BYTES = 64;
export const NAME_MAX_CHARS = 32;
export const INVITE_TOKEN_CHARS = 26;

const HOUR = 3600;
export const INVITE_DEFAULT_TTL_SECS = HOUR;
export const INVITE_MAX_TTL_SECS = 30 * 24 * HOUR;
export const INVITE_DEFAULT_USES = 5;
export const INVITE_MAX_USES = 100;

export type ClientMessage =
  | {
      type: "auth";
      /** The public key's raw bytes and its canonical code (no dashes). */
      key: Uint8Array;
      keyCode: string;
      signature: Uint8Array;
      name: string;
      create: boolean;
      invite: string | null;
    }
  | { type: "invite.create"; ttlSecs: number; maxUses: number }
  | { type: "invite.list" }
  | { type: "invite.revoke"; id: string }
  | { type: "member.remove"; keyCode: string }
  | { type: "room.leave" }
  | { type: "room.enter" }
  | { type: "room.exit" }
  | { type: "screen.start"; screen: ScreenInfo }
  | { type: "screen.stop" }
  | { type: "signal"; toKeyCode: string; envelope: string }
  | { type: "debug.stats" };

/** Largest signed envelope relayed (an offer is about 3 KB). */
export const MAX_ENVELOPE_BYTES = 16 * 1024;

export type ScreenKind = "desktop" | "window";

/** What a participant says about the screen they share. */
export interface ScreenInfo {
  kind: ScreenKind;
  /** A short name, e.g. the app's name ("Valorant") or "Monitor 1". */
  name: string;
  width: number;
  height: number;
  fps: number;
}

export const SCREEN_NAME_MAX_CHARS = 64;
const MIN_SIDE = 16;
const MAX_SIDE = 16_384;
const MAX_FPS = 240;

export type Parsed = ClientMessage | { error: string };

export function parse(text: string): Parsed {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return { error: "not JSON" };
  }
  if (typeof value !== "object" || value === null) {
    return { error: "not an object" };
  }
  const m = value as Record<string, unknown>;
  switch (m.type) {
    case "auth":
      return parseAuth(m);
    case "invite.create": {
      const ttlSecs = m.ttl_secs ?? INVITE_DEFAULT_TTL_SECS;
      const maxUses = m.max_uses ?? INVITE_DEFAULT_USES;
      if (!isInteger(ttlSecs, 60, INVITE_MAX_TTL_SECS)) {
        return { error: `ttl_secs must be 60 to ${INVITE_MAX_TTL_SECS}` };
      }
      if (!isInteger(maxUses, 1, INVITE_MAX_USES)) {
        return { error: `max_uses must be 1 to ${INVITE_MAX_USES}` };
      }
      return { type: "invite.create", ttlSecs, maxUses };
    }
    case "invite.list":
      return { type: "invite.list" };
    case "invite.revoke":
      return typeof m.id === "string" && /^[0-9a-f]{16}$/.test(m.id)
        ? { type: "invite.revoke", id: m.id }
        : { error: "bad invite id" };
    case "member.remove": {
      const key = typeof m.key === "string" ? keyCode(m.key) : null;
      return key ? { type: "member.remove", keyCode: key } : { error: "bad key" };
    }
    case "room.leave":
      return { type: "room.leave" };
    case "room.enter":
      return { type: "room.enter" };
    case "room.exit":
      return { type: "room.exit" };
    case "screen.start":
      return parseScreen(m.screen);
    case "screen.stop":
      return { type: "screen.stop" };
    case "debug.stats":
      return { type: "debug.stats" };
    case "signal": {
      const to = typeof m.to === "string" ? keyCode(m.to) : null;
      if (!to) {
        return { error: "bad recipient" };
      }
      if (typeof m.envelope !== "string" || m.envelope.length === 0) {
        return { error: "bad envelope" };
      }
      if (new TextEncoder().encode(m.envelope).byteLength > MAX_ENVELOPE_BYTES) {
        return { error: `envelope over ${MAX_ENVELOPE_BYTES} bytes` };
      }
      return { type: "signal", toKeyCode: to, envelope: m.envelope };
    }
    default:
      return { error: "unknown message" };
  }
}

function parseAuth(m: Record<string, unknown>): Parsed {
  const key = typeof m.key === "string" ? base32Decode(m.key) : null;
  if (!key || key.length !== PUBLIC_KEY_BYTES) {
    return { error: "bad key" };
  }
  const signature = typeof m.signature === "string" ? hexDecode(m.signature) : null;
  if (!signature || signature.length !== SIGNATURE_BYTES) {
    return { error: "bad signature" };
  }
  const name = typeof m.name === "string" ? cleanName(m.name) : "";
  if (name === "") {
    return { error: "bad name" };
  }
  if (m.create !== undefined && typeof m.create !== "boolean") {
    return { error: "bad create flag" };
  }
  let invite: string | null = null;
  if (m.invite !== undefined) {
    if (typeof m.invite !== "string" || !/^[a-z2-7]+$/.test(m.invite) || m.invite.length !== INVITE_TOKEN_CHARS) {
      return { error: "bad invite" };
    }
    invite = m.invite;
  }
  return {
    type: "auth",
    key,
    keyCode: base32Encode(key),
    signature,
    name,
    create: m.create === true,
    invite,
  };
}

/** A key code in canonical form (lowercase, no dashes), or `null`. */
export function keyCode(text: string): string | null {
  const bytes = base32Decode(text);
  return bytes && bytes.length === PUBLIC_KEY_BYTES ? base32Encode(bytes) : null;
}

/** A display name without control characters or repeated spaces, cut to
 * `NAME_MAX_CHARS` characters. */
export function cleanName(text: string, maxChars = NAME_MAX_CHARS): string {
  const cleaned = text
    .replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, "")
    .replace(/\s+/g, " ")
    .trim();
  return Array.from(cleaned).slice(0, maxChars).join("").trim();
}

function parseScreen(value: unknown): Parsed {
  if (typeof value !== "object" || value === null) {
    return { error: "bad screen" };
  }
  const s = value as Record<string, unknown>;
  if (s.kind !== "desktop" && s.kind !== "window") {
    return { error: "bad screen kind" };
  }
  const name = typeof s.name === "string" ? cleanName(s.name, SCREEN_NAME_MAX_CHARS) : "";
  if (name === "") {
    return { error: "bad screen name" };
  }
  if (!isInteger(s.width, MIN_SIDE, MAX_SIDE) || !isInteger(s.height, MIN_SIDE, MAX_SIDE)) {
    return { error: `screen width and height must be ${MIN_SIDE} to ${MAX_SIDE}` };
  }
  if (!isInteger(s.fps, 1, MAX_FPS)) {
    return { error: `screen fps must be 1 to ${MAX_FPS}` };
  }
  return { type: "screen.start", screen: { kind: s.kind, name, width: s.width, height: s.height, fps: s.fps } };
}

function isInteger(value: unknown, min: number, max: number): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= min && value <= max;
}
