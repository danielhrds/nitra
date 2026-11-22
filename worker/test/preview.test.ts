// Screen previews: a sharer's sealed still image, relayed as is to everyone
// else in the room (never stored), at most one per socket every few seconds.

import { describe, expect, it } from "vitest";

import { MAX_PREVIEW_CHARS, parse } from "../src/protocol";
import { type Client, type Person as PersonType, Person, roomWithInvite } from "./helpers";

const SCREEN = { kind: "window", name: "Valorant", width: 1920, height: 1080, fps: 60 };
/** Opaque to the room (the app seals it end to end): any base64. */
const DATA = "c2VhbGVkIHByZXZpZXc=";

/** alice (owner) and bob connected, bob sharing a screen. */
async function bobSharing(): Promise<{ room: string; alice: PersonType; bob: PersonType; a: Client; b: Client }> {
  const alice = await Person.make("alice");
  const bob = await Person.make("bob");
  const { room, ownerClient: a, token } = await roomWithInvite(alice);
  const { client: b } = await bob.enter(room, { invite: token });
  await a.expect("member.joined");
  b.send({ type: "screen.start", screen: SCREEN });
  // bob coming online is announced first; then his screen.
  for (;;) {
    const change = await a.expect("presence.changed");
    if (change.key === bob.key && change.screen) {
      break;
    }
  }
  return { room, alice, bob, a, b };
}

describe("screen previews", () => {
  it("validate as opaque base64 of bounded size", () => {
    expect(parse(JSON.stringify({ type: "screen.preview", data: DATA }))).toEqual({
      type: "screen.preview",
      data: DATA,
    });
    const bad = (data: unknown) => parse(JSON.stringify({ type: "screen.preview", data }));
    expect(bad(undefined)).toEqual({ error: "bad preview" });
    expect(bad("")).toEqual({ error: "bad preview" });
    expect(bad("not base64!")).toEqual({ error: "bad preview" });
    expect(bad("A".repeat(MAX_PREVIEW_CHARS + 4))).toHaveProperty("error");
    expect(bad("A".repeat(MAX_PREVIEW_CHARS))).toEqual({ type: "screen.preview", data: "A".repeat(MAX_PREVIEW_CHARS) });
  });

  it("go from a sharer to everyone else, as sent, and not back", async () => {
    const { room, bob, a, b } = await bobSharing();
    const carol = await Person.make("carol");
    // carol joins with a second invite from alice.
    a.send({ type: "invite.create" });
    const invite = await a.expect("invite.created");
    const { client: c } = await carol.enter(room, { invite: invite.token });
    await a.expect("member.joined");
    await b.expect("member.joined");
    b.send({ type: "screen.preview", data: DATA });
    expect(await a.expect("screen.preview")).toEqual({ type: "screen.preview", from: bob.key, data: DATA });
    expect(await c.expect("screen.preview")).toEqual({ type: "screen.preview", from: bob.key, data: DATA });
    // Not back to the sender, on any of its sockets.
    const { client: b2 } = await bob.enter(room);
    await b.expect("presence.changed").catch(() => undefined);
    expect(await b.quiet()).toBe(true);
    expect(await b2.quiet()).toBe(true);
  });

  it("fit the message limit even at the largest size", async () => {
    const { bob, a, b } = await bobSharing();
    const largest = "A".repeat(MAX_PREVIEW_CHARS);
    b.send({ type: "screen.preview", data: largest });
    expect(await a.expect("screen.preview")).toEqual({ type: "screen.preview", from: bob.key, data: largest });
    expect(b.closed).toBeUndefined();
  });

  it("need a screen being shared", async () => {
    const { a } = await bobSharing();
    a.send({ type: "screen.preview", data: DATA });
    expect(await a.expect("error")).toEqual({ type: "error", reason: "share a screen first" });
  });

  it("are limited to one per socket every few seconds", async () => {
    const { bob, a, b } = await bobSharing();
    b.send({ type: "screen.preview", data: DATA });
    expect(await a.expect("screen.preview")).toMatchObject({ from: bob.key });
    b.send({ type: "screen.preview", data: DATA });
    expect(await b.expect("error")).toEqual({ type: "error", reason: "too many previews, wait a moment" });
    expect(await a.quiet()).toBe(true);
  });

  it("refuse a bad one without closing", async () => {
    const { a, b } = await bobSharing();
    b.send({ type: "screen.preview", data: "%%%" });
    expect(await b.expect("error")).toEqual({ type: "error", reason: "bad preview" });
    expect(b.closed).toBeUndefined();
    expect(await a.quiet()).toBe(true);
  });
});
