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
  | { type: "room.leave" };

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
export function cleanName(text: string): string {
  const cleaned = text
    .replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, "")
    .replace(/\s+/g, " ")
    .trim();
  return Array.from(cleaned).slice(0, NAME_MAX_CHARS).join("").trim();
}

function isInteger(value: unknown, min: number, max: number): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= min && value <= max;
}
