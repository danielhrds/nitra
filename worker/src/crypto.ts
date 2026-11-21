// Encodings and signatures shared with the desktop app: base32 (RFC 4648,
// lowercase, no padding; the alphabet of the app's key codes), hex, random
// bytes, SHA-256 and Ed25519 verification with the runtime's WebCrypto.

const ALPHABET = "abcdefghijklmnopqrstuvwxyz234567";

export function base32Encode(bytes: Uint8Array): string {
  let out = "";
  let buffer = 0;
  let bits = 0;
  for (const byte of bytes) {
    buffer = (buffer << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      out += ALPHABET[(buffer >> bits) & 31];
    }
    buffer &= (1 << bits) - 1;
  }
  if (bits > 0) {
    out += ALPHABET[(buffer << (5 - bits)) & 31];
  }
  return out;
}

/** Decodes base32, ignoring dashes, whitespace and case (people copy key
 * codes in groups); `null` for anything else. */
export function base32Decode(text: string): Uint8Array | null {
  const out: number[] = [];
  let buffer = 0;
  let bits = 0;
  for (const c of text.toLowerCase()) {
    if (c === "-" || /\s/.test(c)) {
      continue;
    }
    const value = ALPHABET.indexOf(c);
    if (value < 0) {
      return null;
    }
    buffer = (buffer << 5) | value;
    bits += 5;
    if (bits >= 8) {
      bits -= 8;
      out.push((buffer >> bits) & 0xff);
    }
    buffer &= (1 << bits) - 1;
  }
  return new Uint8Array(out);
}

export function hexEncode(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

export function hexDecode(text: string): Uint8Array | null {
  if (text.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(text)) {
    return null;
  }
  const out = new Uint8Array(text.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = Number.parseInt(text.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

export function randomBytes(length: number): Uint8Array {
  return crypto.getRandomValues(new Uint8Array(length));
}

export async function sha256Hex(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return hexEncode(new Uint8Array(digest));
}

/** What a participant signs to enter a room: a domain prefix (so the
 * signature means nothing anywhere else), the room id and the room's
 * one-time challenge for this connection. */
export function authMessage(room: string, challenge: Uint8Array): Uint8Array {
  const prefix = new TextEncoder().encode(`ziz room auth v1\0${room}\0`);
  const message = new Uint8Array(prefix.length + challenge.length);
  message.set(prefix);
  message.set(challenge, prefix.length);
  return message;
}

/** Whether `signature` is `publicKey`'s Ed25519 signature of `message`;
 * malformed keys or signatures are just `false`. */
export async function verifyEd25519(
  publicKey: Uint8Array,
  message: Uint8Array,
  signature: Uint8Array,
): Promise<boolean> {
  try {
    const key = await crypto.subtle.importKey("raw", publicKey, { name: "Ed25519" }, false, ["verify"]);
    return await crypto.subtle.verify({ name: "Ed25519" }, key, signature, message);
  } catch {
    return false;
  }
}
