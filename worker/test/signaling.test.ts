// Signaling: signed envelopes relayed unchanged between participants, with
// the sender set by the room, refusals and a rate limit.

import { evictDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { MAX_ENVELOPE_BYTES, parse } from "../src/protocol";
import { type Room, SIGNAL_LIMIT } from "../src/room";
import { Client, type Json, Person, roomWithInvite } from "./helpers";

/** Looks like the app's signed envelope; the room never reads it. */
const ENVELOPE = JSON.stringify({
  body: '{"from":"x","to":"y","nonce":"00","message":{"Offer":{"sdp":"v=0\\r\\n…","profile":"bob"}}}',
  signature: "ab".repeat(64),
});

function stubOf(room: string): DurableObjectStub<Room> {
  return env.ROOMS.get(env.ROOMS.idFromName(room));
}

async function aliceAndBob(): Promise<{ room: string; alice: Person; bob: Person; a: Client; b: Client }> {
  const alice = await Person.make("alice");
  const bob = await Person.make("bob");
  const { room, ownerClient: a, token } = await roomWithInvite(alice);
  const { client: b } = await bob.enter(room, { invite: token });
  await a.expect("member.joined");
  return { room, alice, bob, a, b };
}

describe("signal validation", () => {
  it("needs a key code and a bounded envelope", () => {
    const to = "a".repeat(52);
    expect(parse(JSON.stringify({ type: "signal", to, envelope: "x" }))).toMatchObject({ type: "signal" });
    expect(parse(JSON.stringify({ type: "signal", to: "bob", envelope: "x" }))).toEqual({ error: "bad recipient" });
    expect(parse(JSON.stringify({ type: "signal", to, envelope: "" }))).toEqual({ error: "bad envelope" });
    expect(parse(JSON.stringify({ type: "signal", to, envelope: { a: 1 } }))).toEqual({ error: "bad envelope" });
    expect(parse(JSON.stringify({ type: "signal", to, envelope: "x".repeat(MAX_ENVELOPE_BYTES + 1) }))).toHaveProperty(
      "error",
    );
  });
});

describe("relaying signals", () => {
  it("delivers the envelope unchanged, from the sender's real key", async () => {
    const { alice, bob, a, b } = await aliceAndBob();
    b.send({ type: "signal", to: alice.key, envelope: ENVELOPE });
    const got = await a.expect("signal");
    expect(got).toEqual({ type: "signal", from: bob.key, envelope: ENVELOPE });
    // And the answer back.
    a.send({ type: "signal", to: bob.key, envelope: "answer" });
    expect(await b.expect("signal")).toEqual({ type: "signal", from: alice.key, envelope: "answer" });
  });

  it("ignores a forged sender", async () => {
    const { alice, bob, a, b } = await aliceAndBob();
    b.send({ type: "signal", to: alice.key, envelope: "x", from: alice.key });
    expect((await a.expect("signal")).from).toBe(bob.key);
  });

  it("accepts the recipient's key code grouped with dashes", async () => {
    const { alice, a, b } = await aliceAndBob();
    b.send({ type: "signal", to: alice.key.toUpperCase().replace(/(.{4})(?!$)/g, "$1-"), envelope: "x" });
    expect((await a.expect("signal")).envelope).toBe("x");
  });

  it("reaches every open socket of the recipient", async () => {
    const { room, alice, a, b } = await aliceAndBob();
    const { client: a2 } = await alice.enter(room);
    b.send({ type: "signal", to: alice.key, envelope: "x" });
    expect((await a.expect("signal")).envelope).toBe("x");
    expect((await a2.expect("signal")).envelope).toBe("x");
  });

  it("refuses offline participants, strangers and yourself, without closing", async () => {
    const { alice, bob, a, b } = await aliceAndBob();
    const stranger = await Person.make("eve");
    b.send({ type: "signal", to: stranger.key, envelope: "x" });
    expect(await b.expect("signal.failed")).toEqual({
      type: "signal.failed",
      to: stranger.key,
      reason: "not a participant",
    });
    b.send({ type: "signal", to: bob.key, envelope: "x" });
    expect(await b.expect("signal.failed")).toMatchObject({ reason: "that is you" });
    a.ws.close(1000);
    await a.closing();
    b.send({ type: "signal", to: alice.key, envelope: "x" });
    expect(await b.expect("signal.failed")).toMatchObject({ to: alice.key, reason: "offline" });
    expect(b.closed).toBeUndefined();
  });

  it("refuses an envelope too big without closing", async () => {
    const { alice, b } = await aliceAndBob();
    b.send({ type: "signal", to: alice.key, envelope: "x".repeat(MAX_ENVELOPE_BYTES + 1) });
    expect(await b.expect("error")).toMatchObject({ reason: `envelope over ${MAX_ENVELOPE_BYTES} bytes` });
    expect(b.closed).toBeUndefined();
  });

  it("limits how many signals a socket sends", async () => {
    const { alice, a, b } = await aliceAndBob();
    for (let i = 0; i < SIGNAL_LIMIT; i++) {
      b.send({ type: "signal", to: alice.key, envelope: `${i}` });
    }
    for (let i = 0; i < SIGNAL_LIMIT; i++) {
      expect((await a.expect("signal")).envelope).toBe(`${i}`);
    }
    b.send({ type: "signal", to: alice.key, envelope: "one too many" });
    expect(await b.expect("signal.failed")).toMatchObject({ reason: "too many signals, wait a moment" });
    expect(await a.quiet()).toBe(true);
  });

  it("does not relay before authentication", async () => {
    const { room, alice } = await aliceAndBob();
    const client = await Client.open(room);
    await client.expect("challenge");
    client.send({ type: "signal", to: alice.key, envelope: "x" });
    expect((await client.closing()).reason).toBe("authenticate first");
  });

  it("works after hibernation", async () => {
    const { room, alice, bob, a, b } = await aliceAndBob();
    await evictDurableObject(stubOf(room));
    b.send({ type: "signal", to: alice.key, envelope: "after sleep" });
    expect(await a.expect("signal")).toMatchObject({ from: bob.key, envelope: "after sleep" } as Json);
  });
});
