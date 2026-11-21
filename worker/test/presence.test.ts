// Presence: online, in the room and sharing a screen, told to everyone
// connected; screens end with their sockets; silent sockets are closed.

import { evictDurableObject, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { OFFLINE, presenceOf } from "../src/presence";
import { parse } from "../src/protocol";
import { type Attachment, CLOSE, LIVENESS_TIMEOUT_MS, type Room } from "../src/room";
import { type Client, type Json, Person, roomWithInvite } from "./helpers";

const SCREEN = { kind: "window", name: "Valorant", width: 1920, height: 1080, fps: 60 };

function stubOf(room: string): DurableObjectStub<Room> {
  return env.ROOMS.get(env.ROOMS.idFromName(room));
}

/** A room with alice (owner) and bob both connected, bob's arrival seen. */
async function aliceAndBob(): Promise<{
  room: string;
  alice: Person;
  bob: Person;
  a: Client;
  b: Client;
}> {
  const alice = await Person.make("alice");
  const bob = await Person.make("bob");
  const { room, ownerClient: a, token } = await roomWithInvite(alice);
  const { client: b } = await bob.enter(room, { invite: token });
  await a.expect("member.joined");
  expect(await a.expect("presence.changed")).toMatchObject({ key: bob.key, online: true, entered: false });
  return { room, alice, bob, a, b };
}

describe("presence rules", () => {
  it("combine a participant's sockets", () => {
    const screen = { ...SCREEN, kind: "desktop" as const, started_at: 1 };
    expect(presenceOf("a", [])).toEqual(OFFLINE);
    expect(
      presenceOf("a", [
        { key: "a", entered: false, screen: null },
        { key: "a", entered: true, screen },
        { key: "b", entered: true, screen: null },
      ]),
    ).toEqual({ online: true, entered: true, screen });
  });

  it("validate screens", () => {
    expect(parse(JSON.stringify({ type: "screen.start", screen: SCREEN }))).toMatchObject({ type: "screen.start" });
    const bad = (screen: unknown) => parse(JSON.stringify({ type: "screen.start", screen }));
    expect(bad(null)).toEqual({ error: "bad screen" });
    expect(bad({ ...SCREEN, kind: "tab" })).toEqual({ error: "bad screen kind" });
    expect(bad({ ...SCREEN, name: "\u0000 " })).toEqual({ error: "bad screen name" });
    expect(bad({ ...SCREEN, width: 8 })).toHaveProperty("error");
    expect(bad({ ...SCREEN, height: 1080.5 })).toHaveProperty("error");
    expect(bad({ ...SCREEN, fps: 0 })).toHaveProperty("error");
    expect(bad({ ...SCREEN, fps: 241 })).toHaveProperty("error");
    const long = parse(JSON.stringify({ type: "screen.start", screen: { ...SCREEN, name: "x".repeat(100) } }));
    expect((long as { screen: { name: string } }).screen.name).toHaveLength(64);
  });
});

describe("presence in a room", () => {
  it("shows who is online in welcome", async () => {
    const alice = await Person.make("alice");
    const bob = await Person.make("bob");
    const { room, token } = await roomWithInvite(alice);
    const { welcome } = await bob.enter(room, { invite: token });
    const presence = welcome.presence as Json[];
    expect(presence).toHaveLength(2);
    expect(presence).toContainEqual({ key: alice.key, online: true, entered: false, screen: null });
    expect(presence).toContainEqual({ key: bob.key, online: true, entered: false, screen: null });
  });

  it("tells everyone when someone enters, shares a screen, stops and exits", async () => {
    const { bob, a, b } = await aliceAndBob();
    b.send({ type: "room.enter" });
    expect(await a.expect("presence.changed")).toEqual({
      type: "presence.changed",
      key: bob.key,
      online: true,
      entered: true,
      screen: null,
    });
    b.send({ type: "screen.start", screen: SCREEN });
    const live = await a.expect("presence.changed");
    expect(live.screen).toMatchObject(SCREEN);
    expect((live.screen as Json).started_at).toBeGreaterThan(0);
    b.send({ type: "screen.stop" });
    expect(await a.expect("presence.changed")).toMatchObject({ entered: true, screen: null });
    b.send({ type: "room.exit" });
    expect(await a.expect("presence.changed")).toMatchObject({ online: true, entered: false, screen: null });
  });

  it("lets a participant who only has the app open see it all", async () => {
    const { a, b } = await aliceAndBob();
    // alice never entered the room, yet she sees bob go live.
    b.send({ type: "screen.start", screen: SCREEN });
    expect(await a.expect("presence.changed")).toMatchObject({ entered: true, screen: { name: "Valorant" } });
  });

  it("shows the screens already live to someone who connects later", async () => {
    const { room, bob, b } = await aliceAndBob();
    b.send({ type: "screen.start", screen: SCREEN });
    await b.expect("presence.changed");
    const { welcome } = await bob.enter(room); // a second socket of bob's
    const mine = (welcome.presence as Json[]).find((p) => p.key === bob.key);
    expect(mine).toMatchObject({ entered: true, screen: { name: "Valorant" } });
  });

  it("keeps one screen per participant: a new one replaces the old", async () => {
    const { room, bob, a, b } = await aliceAndBob();
    b.send({ type: "screen.start", screen: SCREEN });
    await a.expect("presence.changed");
    const { client: b2 } = await bob.enter(room);
    b2.send({ type: "screen.start", screen: { ...SCREEN, kind: "desktop", name: "Monitor 1" } });
    expect(await a.expect("presence.changed")).toMatchObject({ screen: { kind: "desktop", name: "Monitor 1" } });
    const states = await runInDurableObject(stubOf(room), (_i, state) =>
      state
        .getWebSockets()
        .map((ws) => ws.deserializeAttachment() as Attachment)
        .filter((at) => at.key === bob.key && at.screen !== undefined),
    );
    expect(states).toHaveLength(1);
  });

  it("ends a screen and the presence when the socket closes", async () => {
    const { bob, a, b } = await aliceAndBob();
    b.send({ type: "screen.start", screen: SCREEN });
    await a.expect("presence.changed");
    b.ws.close(1000, "bye");
    expect(await a.expect("presence.changed")).toEqual({
      type: "presence.changed",
      key: bob.key,
      online: false,
      entered: false,
      screen: null,
    });
  });

  it("says nothing more about someone removed (member.removed says it)", async () => {
    const { bob, a, b } = await aliceAndBob();
    b.send({ type: "screen.start", screen: SCREEN });
    await a.expect("presence.changed");
    a.send({ type: "member.remove", key: bob.key });
    expect(await a.expect("member.removed")).toMatchObject({ key: bob.key });
    await b.closing();
    expect(await a.quiet(300)).toBe(true);
  });

  it("closes silent sockets and tells the others, and keeps active ones", async () => {
    const { room, bob, a } = await aliceAndBob();
    // bob's socket last spoke long ago (and never sent a keepalive).
    await runInDurableObject(stubOf(room), (_i, state) => {
      for (const ws of state.getWebSockets()) {
        const at = ws.deserializeAttachment() as Attachment;
        if (at.key === bob.key) {
          ws.serializeAttachment({ ...at, lastSeen: Date.now() - LIVENESS_TIMEOUT_MS - 1 });
        }
      }
    });
    expect(await runDurableObjectAlarm(stubOf(room))).toBe(true);
    expect(await a.expect("presence.changed")).toMatchObject({ key: bob.key, online: false });
    expect(await a.quiet()).toBe(true);
  });

  it("counts the keepalive as life", async () => {
    const { room, bob, b, a } = await aliceAndBob();
    b.send("ping");
    expect(await b.next()).toBe("pong");
    await runInDurableObject(stubOf(room), (_i, state) => {
      for (const ws of state.getWebSockets()) {
        const at = ws.deserializeAttachment() as Attachment;
        if (at.key === bob.key) {
          ws.serializeAttachment({ ...at, lastSeen: 0 });
        }
      }
    });
    await runDurableObjectAlarm(stubOf(room));
    expect(await a.quiet()).toBe(true);
    expect(b.closed).toBeUndefined();
  });

  it("schedules the liveness check while anyone is connected", async () => {
    const { room } = await aliceAndBob();
    const alarm = await runInDurableObject(stubOf(room), (_i, state) => state.storage.getAlarm());
    expect(alarm).not.toBeNull();
  });

  it("survives hibernation", async () => {
    const { room, bob, a, b } = await aliceAndBob();
    b.send({ type: "screen.start", screen: SCREEN });
    await a.expect("presence.changed");
    await evictDurableObject(stubOf(room));
    b.send({ type: "screen.stop" });
    expect(await a.expect("presence.changed")).toMatchObject({ key: bob.key, entered: true, screen: null });
  });

  it("refuses bad screens without closing", async () => {
    const { b } = await aliceAndBob();
    b.send({ type: "screen.start", screen: { ...SCREEN, fps: 1000 } });
    expect(await b.expect("error")).toMatchObject({ reason: "screen fps must be 1 to 240" });
    expect(b.closed).toBeUndefined();
  });

  it("refuses the dev-only stats outside the dev deployment", async () => {
    const { b } = await aliceAndBob();
    b.send({ type: "debug.stats" });
    expect(await b.expect("error")).toMatchObject({ reason: "unknown message" });
  });

  it("uses a close code of its own for silent sockets", () => {
    expect(CLOSE.silent).toBe(4009);
  });
});
