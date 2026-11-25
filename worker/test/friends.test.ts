// Friendships: rooms of two, with one single-use invite, that end when
// either one leaves.

import { describe, expect, it } from "vitest";

import { CLOSE } from "../src/room";
import { type Client, type Json, Person, roomId } from "./helpers";

/** A friendship `alice` started, with its invite token. */
async function friendship(alice: Person, invite: Json = {}): Promise<{ room: string; a: Client; token: string }> {
  const room = roomId();
  const { client, welcome } = await alice.enter(room, { create: true, kind: "friend" });
  expect(welcome.room).toEqual({ name: "Test room", kind: "friend", ended: false });
  client.send({ type: "invite.create", ttl_secs: 7 * 24 * 3600, ...invite });
  const created = await client.expect("invite.created");
  return { room, a: client, token: created.token as string };
}

describe("friendships", () => {
  it("are rooms of two whose invite has one use", async () => {
    const [alice, bob] = await Promise.all([Person.make("alice"), Person.make("bob")]);
    const { room, a, token } = await friendship(alice, { max_uses: 5 });
    const { welcome } = await bob.enter(room, { invite: token });
    expect(welcome.room).toEqual({ name: "Test room", kind: "friend", ended: false });
    expect(welcome.members).toHaveLength(2);
    expect((await a.expect("member.joined")).member).toMatchObject({ key: bob.key });
  });

  it("never take a third person, even with an invite", async () => {
    const [alice, bob, carol] = await Promise.all([Person.make("alice"), Person.make("bob"), Person.make("carol")]);
    const { room, a, token } = await friendship(alice);
    await bob.enter(room, { invite: token });
    // The used invite is gone, and no new one is given.
    const late = await carol.connect(room, { invite: token });
    expect(await late.closing()).toEqual({ code: CLOSE.notAllowed, reason: "the friendship already has two people" });
    a.send({ type: "invite.create" });
    expect(await a.expect("error")).toMatchObject({ reason: "the friendship takes no more invites" });
  });

  it("keep one invite at a time: a new one replaces the old", async () => {
    const [alice, bob] = await Promise.all([Person.make("alice"), Person.make("bob")]);
    const { room, a, token: first } = await friendship(alice);
    a.send({ type: "invite.create" });
    const second = (await a.expect("invite.created")).token as string;
    const old = await bob.connect(room, { invite: first });
    expect((await old.closing()).code).toBe(CLOSE.notAllowed);
    await bob.enter(room, { invite: second });
  });

  it("are never renamed and nobody is removed from them", async () => {
    const [alice, bob] = await Promise.all([Person.make("alice"), Person.make("bob")]);
    const { room, a, token } = await friendship(alice);
    await bob.enter(room, { invite: token });
    a.send({ type: "room.rename", name: "Other" });
    expect(await a.expect("error")).toMatchObject({ reason: "a friendship has no name to change" });
    a.send({ type: "member.remove", key: bob.key });
    expect(await a.expect("error")).toMatchObject({ reason: "a friendship ends with room.leave" });
  });

  it("end for both when one leaves, and go away when the other leaves too", async () => {
    const [alice, bob] = await Promise.all([Person.make("alice"), Person.make("bob")]);
    const { room, a, token } = await friendship(alice);
    const b = await bob.enter(room, { invite: token });
    await a.expect("member.joined");
    // Bob goes offline; alice ends the friendship meanwhile.
    b.client.ws.close(1000);
    await b.client.closing();
    a.send({ type: "room.leave" });
    expect((await a.closing()).code).toBe(1000);
    // Bob learns it when he comes back, and leaves too: the room is gone.
    const back = await bob.enter(room);
    expect(back.welcome.room).toEqual({ name: "Test room", kind: "friend", ended: true });
    back.client.send({ type: "invite.create" });
    expect(await back.client.expect("error")).toMatchObject({ reason: "the friendship takes no more invites" });
    back.client.send({ type: "room.leave" });
    await back.client.closing();
    const gone = await bob.connect(room);
    expect(await gone.closing()).toEqual({ code: CLOSE.notAllowed, reason: "not a participant" });
  });

  it("tell the one online at once", async () => {
    const [alice, bob] = await Promise.all([Person.make("alice"), Person.make("bob")]);
    const { room, a, token } = await friendship(alice);
    const b = await bob.enter(room, { invite: token });
    await a.expect("member.joined");
    b.client.send({ type: "room.leave" });
    expect(await a.expect("friend.ended")).toEqual({ type: "friend.ended", by: bob.key });
  });

  it("are only asked for when creating a room", async () => {
    const alice = await Person.make("alice");
    const client = await alice.connect(roomId(), { kind: "friend" });
    expect((await client.closing()).code).toBe(CLOSE.authFailed);
    const odd = await alice.connect(roomId(), { create: true, kind: "group" });
    expect((await odd.closing()).code).toBe(CLOSE.authFailed);
  });

  it("leave plain rooms as they were", async () => {
    const alice = await Person.make("alice");
    const { welcome } = await alice.enter(roomId(), { create: true });
    expect(welcome.room).toEqual({ name: "Test room", kind: "room" });
  });
});
