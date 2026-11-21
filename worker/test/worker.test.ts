// The Worker's routes, the shared encodings and message validation.

import { describe, expect, it } from "vitest";

import { authMessage, base32Decode, base32Encode, hexDecode, hexEncode, verifyEd25519 } from "../src/crypto";
import { cleanName, keyCode, parse } from "../src/protocol";
import { Client, get, Person, roomId } from "./helpers";

describe("worker routes", () => {
  it("answers health checks", async () => {
    const response = await get("/health");
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ ok: true });
    expect((await get("/health", { method: "POST" })).status).toBe(405);
  });

  it("refuses unknown paths", async () => {
    expect((await get("/")).status).toBe(404);
    expect((await get("/rooms")).status).toBe(404);
    expect((await get(`/rooms/${roomId()}/other`)).status).toBe(404);
  });

  it("refuses malformed room ids", async () => {
    const upgrade = { headers: { Upgrade: "websocket" } };
    for (const bad of ["abc", "A".repeat(26), "a".repeat(27), "0".repeat(26), `${"a".repeat(25)}1`]) {
      expect((await get(`/rooms/${bad}/ws`, upgrade)).status, bad).toBe(400);
    }
  });

  it("asks for a websocket upgrade", async () => {
    expect((await get(`/rooms/${roomId()}/ws`)).status).toBe(426);
  });

  it("answers keepalives on any socket without waking the room", async () => {
    const client = await Client.open(roomId());
    await client.expect("challenge");
    client.send("ping");
    expect(await client.next()).toBe("pong");
  });

  it("does not let a client choose the room id the room sees", async () => {
    const alice = await Person.make("alice");
    const room = roomId();
    const response = await get(`/rooms/${room}/ws`, { headers: { Upgrade: "websocket", "X-Ziz-Room": roomId() } });
    const client = new Client(response.webSocket as WebSocket);
    const challenge = await client.expect("challenge");
    // Signed for the room in the URL, as an honest app does: accepted.
    const signature = await alice.sign(authMessage(room, hexDecode(challenge.challenge as string) as Uint8Array));
    client.send({ type: "auth", key: alice.key, signature, name: "alice", create: true, room_name: "Test room" });
    expect((await client.nextJson()).type).toBe("welcome");
  });
});

describe("encodings", () => {
  it("match RFC 4648 base32 (lowercase, no padding)", () => {
    const vectors: [string, string][] = [
      ["", ""],
      ["f", "my"],
      ["fo", "mzxq"],
      ["foo", "mzxw6"],
      ["foob", "mzxw6yq"],
      ["fooba", "mzxw6ytb"],
      ["foobar", "mzxw6ytboi"],
    ];
    for (const [plain, encoded] of vectors) {
      const bytes = new TextEncoder().encode(plain);
      expect(base32Encode(bytes)).toBe(encoded);
      expect(base32Decode(encoded)).toEqual(bytes);
    }
    expect(base32Decode("MZXW-6YTB OI")).toEqual(new TextEncoder().encode("foobar"));
    expect(base32Decode("mzxw1")).toBeNull();
  });

  it("round-trip hex", () => {
    const bytes = new Uint8Array([0, 1, 254, 255]);
    expect(hexEncode(bytes)).toBe("0001feff");
    expect(hexDecode("0001FEFF")).toEqual(bytes);
    expect(hexDecode("abc")).toBeNull();
    expect(hexDecode("zz")).toBeNull();
  });

  it("accept the entry signature the app makes (shared vector)", async () => {
    // Made by the Rust client (crates/rooms/tests/auth_vector.rs) with a
    // fixed test key: room "abcdefghijklmnopqrstuvwxyz", challenge 0..31.
    const key = base32Decode("5jfgyy7ctrjavpxvkb5rglwf7gkuo5vox27hxescd3vgsfcg2iwa") as Uint8Array;
    const signature = hexDecode(
      "16d568038ae6ef09158570cddbc7bb945d86f166d7080162ec111a08787b38ab" +
        "67ba0a91534937630de3cb24f07dbccf8d8e8b8ee7b1ebdc2b85c8434880bf0c",
    ) as Uint8Array;
    const challenge = Uint8Array.from({ length: 32 }, (_, i) => i);
    expect(await verifyEd25519(key, authMessage("abcdefghijklmnopqrstuvwxyz", challenge), signature)).toBe(true);
    expect(await verifyEd25519(key, authMessage("abcdefghijklmnopqrstuvwxyy", challenge), signature)).toBe(false);
  });

  it("verify Ed25519 signatures and refuse broken ones", async () => {
    const alice = await Person.make("alice");
    const key = base32Decode(alice.key) as Uint8Array;
    const message = authMessage(roomId(), new Uint8Array(32));
    const signature = hexDecode(await alice.sign(message)) as Uint8Array;
    expect(await verifyEd25519(key, message, signature)).toBe(true);
    const tampered = message.slice();
    tampered[0] = (tampered[0] ?? 0) ^ 1;
    expect(await verifyEd25519(key, tampered, signature)).toBe(false);
    expect(await verifyEd25519(new Uint8Array(32), message, signature)).toBe(false);
    expect(await verifyEd25519(key, message, new Uint8Array(64))).toBe(false);
  });
});

describe("message validation", () => {
  it("cleans names", () => {
    expect(cleanName("  Ana\u0000 \n Paula  ")).toBe("Ana Paula");
    expect(cleanName("a".repeat(40))).toBe("a".repeat(32));
    expect(cleanName("​\u0007")).toBe("");
    expect(Array.from(cleanName("😀".repeat(40)))).toHaveLength(32);
  });

  it("accepts key codes with dashes and any case", async () => {
    const alice = await Person.make("alice");
    const grouped = alice.key.toUpperCase().replace(/(.{4})/g, "$1-");
    expect(keyCode(grouped)).toBe(alice.key);
    expect(keyCode("abcd")).toBeNull();
  });

  it("refuses malformed messages", () => {
    const signature = "00".repeat(64);
    const key = "a".repeat(52);
    expect(parse("{")).toEqual({ error: "not JSON" });
    expect(parse("3")).toEqual({ error: "not an object" });
    expect(parse('{"type":"dance"}')).toEqual({ error: "unknown message" });
    expect(parse(JSON.stringify({ type: "auth", key: "short", signature, name: "a" }))).toEqual({ error: "bad key" });
    expect(parse(JSON.stringify({ type: "auth", key, signature: "00", name: "a" }))).toEqual({
      error: "bad signature",
    });
    expect(parse(JSON.stringify({ type: "auth", key, signature, name: " " }))).toEqual({ error: "bad name" });
    expect(parse(JSON.stringify({ type: "auth", key, signature, name: "a", invite: "x" }))).toEqual({
      error: "bad invite",
    });
    expect(parse(JSON.stringify({ type: "invite.create", ttl_secs: 31 * 86400 }))).toHaveProperty("error");
    expect(parse(JSON.stringify({ type: "invite.create", max_uses: 0 }))).toHaveProperty("error");
    expect(parse(JSON.stringify({ type: "invite.create", max_uses: 101 }))).toHaveProperty("error");
    expect(parse(JSON.stringify({ type: "invite.create", max_uses: 2.5 }))).toHaveProperty("error");
    expect(parse(JSON.stringify({ type: "invite.revoke", id: "nope" }))).toEqual({ error: "bad invite id" });
  });

  it("fills invite defaults: 1 hour and 5 uses", () => {
    expect(parse('{"type":"invite.create"}')).toEqual({ type: "invite.create", ttlSecs: 3600, maxUses: 5 });
  });
});
