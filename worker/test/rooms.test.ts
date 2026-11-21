// Rooms: entering by signature, invites, the owner's powers, leaving, and
// what survives restarts and hibernation.

import { evictDurableObject, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { authMessage, hexDecode } from "../src/crypto";
import { type Attachment, AUTH_TIMEOUT_MS, CLOSE, MAX_MESSAGE_BYTES, type Room, SCHEMA_VERSION } from "../src/room";
import { Client, Person, roomId, roomWithInvite } from "./helpers";

function stubOf(room: string): DurableObjectStub<Room> {
  return env.ROOMS.get(env.ROOMS.idFromName(room));
}

/** The room's own tables (not the runtime's internal ones). */
function tables(room: string): Promise<string[]> {
  return runInDurableObject(stubOf(room), (_instance, state) =>
    state.storage.sql
      .exec<{ name: string }>("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE '_cf_%'")
      .toArray()
      .map((row) => row.name),
  );
}

describe("entering a room", () => {
  it("sends each socket a fresh challenge", async () => {
    const room = roomId();
    const a = await (await Client.open(room)).expect("challenge");
    const b = await (await Client.open(room)).expect("challenge");
    expect(a.challenge).toMatch(/^[0-9a-f]{64}$/);
    expect(a.challenge).not.toBe(b.challenge);
  });

  it("creates a room whose creator is the owner", async () => {
    const alice = await Person.make("alice");
    const room = roomId();
    const { welcome } = await alice.enter(room, { create: true });
    expect(welcome.you).toMatchObject({ key: alice.key, name: "alice", role: "owner" });
    expect(welcome.members).toHaveLength(1);
  });

  it("does not create a room twice", async () => {
    const alice = await Person.make("alice");
    const bob = await Person.make("bob");
    const room = roomId();
    await alice.enter(room, { create: true });
    const client = await bob.connect(room, { create: true });
    expect((await client.closing()).code).toBe(CLOSE.notAllowed);
  });

  it("refuses strangers, and an unknown room leaves no data", async () => {
    const bob = await Person.make("bob");
    const room = roomId();
    const client = await bob.connect(room);
    expect(await client.closing()).toEqual({ code: CLOSE.notAllowed, reason: "not a participant" });
    expect(await tables(room)).toEqual([]);
  });

  it("refuses a signature of another challenge or another room", async () => {
    const alice = await Person.make("alice");
    const room = roomId();
    for (const [signedRoom, signedChallenge] of [
      [room, new Uint8Array(32)],
      [roomId(), null],
    ] as const) {
      const client = await Client.open(room);
      const challenge = await client.expect("challenge");
      const bytes = signedChallenge ?? (hexDecode(challenge.challenge as string) as Uint8Array);
      const signature = await alice.sign(authMessage(signedRoom, bytes));
      client.send({ type: "auth", key: alice.key, signature, name: "alice", create: true });
      expect(await client.closing()).toEqual({ code: CLOSE.authFailed, reason: "bad signature" });
    }
  });

  it("refuses a signature made by another key", async () => {
    const alice = await Person.make("alice");
    const mallory = await Person.make("mallory");
    const room = roomId();
    const client = await Client.open(room);
    const challenge = await client.expect("challenge");
    const signature = await mallory.sign(authMessage(room, hexDecode(challenge.challenge as string) as Uint8Array));
    client.send({ type: "auth", key: alice.key, signature, name: "alice", create: true });
    expect((await client.closing()).code).toBe(CLOSE.authFailed);
  });

  it("closes sockets that talk before authenticating", async () => {
    const client = await Client.open(roomId());
    await client.expect("challenge");
    client.send({ type: "invite.list" });
    expect(await client.closing()).toEqual({ code: CLOSE.authFailed, reason: "authenticate first" });
  });

  it("lets participants come back without an invite, with their new name", async () => {
    const alice = await Person.make("alice");
    const room = roomId();
    const first = await alice.enter(room, { create: true });
    first.client.ws.close(1000);
    await first.client.closing();
    const { welcome } = await alice.enter(room, { name: "Alice B." });
    expect(welcome.you).toMatchObject({ key: alice.key, name: "Alice B.", role: "owner" });
  });

  it("accepts the key code grouped with dashes", async () => {
    const alice = await Person.make("alice");
    const room = roomId();
    const client = await Client.open(room);
    const challenge = await client.expect("challenge");
    const signature = await alice.sign(authMessage(room, hexDecode(challenge.challenge as string) as Uint8Array));
    const grouped = alice.key.replace(/(.{4})(?!$)/g, "$1-");
    client.send({ type: "auth", key: grouped, signature, name: "alice", create: true });
    expect((await client.nextJson()).you).toMatchObject({ key: alice.key });
  });

  it("closes a socket that sends a message too big", async () => {
    const client = await Client.open(roomId());
    await client.expect("challenge");
    client.send("x".repeat(MAX_MESSAGE_BYTES + 1));
    expect((await client.closing()).code).toBe(1009);
  });

  it("closes sockets that do not authenticate in time, and only those", async () => {
    const alice = await Person.make("alice");
    const room = roomId();
    const { client: member } = await alice.enter(room, { create: true });
    const idle = await Client.open(room);
    await idle.expect("challenge");
    // Make the idle socket look older than the timeout, then run the alarm.
    await runInDurableObject(stubOf(room), (_instance, state) => {
      for (const ws of state.getWebSockets()) {
        const attachment = ws.deserializeAttachment() as Attachment;
        if (attachment.key === undefined) {
          ws.serializeAttachment({ ...attachment, connectedAt: Date.now() - AUTH_TIMEOUT_MS - 1 });
        }
      }
    });
    expect(await runDurableObjectAlarm(stubOf(room))).toBe(true);
    expect(await idle.closing()).toEqual({ code: CLOSE.authTimeout, reason: "authentication timeout" });
    expect(await member.quiet()).toBe(true);
  });
});

describe("invites", () => {
  it("let a new participant in, using one use, and tell the others", async () => {
    const alice = await Person.make("alice");
    const bob = await Person.make("bob");
    const { room, ownerClient, token } = await roomWithInvite(alice);
    const { welcome } = await bob.enter(room, { invite: token });
    expect(welcome.you).toMatchObject({ key: bob.key, role: "member" });
    expect(welcome.members).toHaveLength(2);
    expect((await ownerClient.expect("member.joined")).member).toMatchObject({ key: bob.key, name: "bob" });
    ownerClient.send({ type: "invite.list" });
    const [invite] = (await ownerClient.expect("invites")).invites as Record<string, unknown>[];
    expect(invite).toMatchObject({ created_by: alice.key, uses_left: 4 });
    const lifetime = (invite?.expires_at as number) - (invite?.created_at as number);
    expect(lifetime).toBe(3600 * 1000);
  });

  it("stop working when their uses run out", async () => {
    const alice = await Person.make("alice");
    const { room, token } = await roomWithInvite(alice, { max_uses: 1 });
    await (await Person.make("bob")).enter(room, { invite: token });
    const carol = await (await Person.make("carol")).connect(room, { invite: token });
    expect(await carol.closing()).toEqual({ code: CLOSE.notAllowed, reason: "invalid invite" });
  });

  it("stop working when they expire", async () => {
    const alice = await Person.make("alice");
    const { room, token } = await roomWithInvite(alice);
    await runInDurableObject(stubOf(room), (_instance, state) => {
      state.storage.sql.exec("UPDATE invites SET expires_at = ?", Date.now() - 1);
    });
    const bob = await (await Person.make("bob")).connect(room, { invite: token });
    expect((await bob.closing()).code).toBe(CLOSE.notAllowed);
  });

  it("can be revoked by the owner only", async () => {
    const alice = await Person.make("alice");
    const bob = await Person.make("bob");
    const { room, ownerClient, inviteId } = await roomWithInvite(alice, { max_uses: 3 });
    ownerClient.send({ type: "invite.create" });
    const second = await ownerClient.expect("invite.created");
    const { client: bobClient } = await bob.enter(room, { invite: second.token });
    bobClient.send({ type: "invite.revoke", id: inviteId });
    expect(await bobClient.expect("error")).toMatchObject({ reason: "only the owner can revoke invites" });
    ownerClient.send({ type: "invite.revoke", id: inviteId });
    expect(await ownerClient.expect("invite.revoked")).toMatchObject({ id: inviteId });
    ownerClient.send({ type: "invite.revoke", id: inviteId });
    expect(await ownerClient.expect("error")).toMatchObject({ reason: "unknown invite" });
  });

  it("refuses a revoked invite", async () => {
    const alice = await Person.make("alice");
    const { room, ownerClient, token, inviteId } = await roomWithInvite(alice);
    ownerClient.send({ type: "invite.revoke", id: inviteId });
    await ownerClient.expect("invite.revoked");
    const bob = await (await Person.make("bob")).connect(room, { invite: token });
    expect((await bob.closing()).code).toBe(CLOSE.notAllowed);
  });

  it("can be created by any participant, with their own limits", async () => {
    const alice = await Person.make("alice");
    const bob = await Person.make("bob");
    const { room, token } = await roomWithInvite(alice);
    const { client } = await bob.enter(room, { invite: token });
    client.send({ type: "invite.create", ttl_secs: 3600, max_uses: 2 });
    const created = await client.expect("invite.created");
    expect(created.token).toMatch(/^[a-z2-7]{26}$/);
    expect(created.uses_left).toBe(2);
    await (await Person.make("carol")).enter(room, { invite: created.token });
  });

  it("are stored only as hashes", async () => {
    const alice = await Person.make("alice");
    const { room, token } = await roomWithInvite(alice);
    const dump = await runInDurableObject(stubOf(room), (_instance, state) =>
      JSON.stringify(state.storage.sql.exec("SELECT * FROM invites").toArray()),
    );
    expect(dump).not.toContain(token);
    expect(dump).toMatch(/"hash":"[0-9a-f]{64}"/);
  });

  it("refuse bad limits and an unknown invite", async () => {
    const alice = await Person.make("alice");
    const { room, ownerClient } = await roomWithInvite(alice);
    ownerClient.send({ type: "invite.create", max_uses: 500 });
    expect(await ownerClient.expect("error")).toMatchObject({ reason: "max_uses must be 1 to 100" });
    const bob = await (await Person.make("bob")).connect(room, { invite: "a".repeat(26) });
    expect(await bob.closing()).toEqual({ code: CLOSE.notAllowed, reason: "invalid invite" });
  });
});

describe("the owner and leaving", () => {
  it("lets the owner remove a participant, who cannot come back without a new invite", async () => {
    const alice = await Person.make("alice");
    const bob = await Person.make("bob");
    const { room, ownerClient, token } = await roomWithInvite(alice);
    const { client: bobClient } = await bob.enter(room, { invite: token });
    await ownerClient.expect("member.joined");
    ownerClient.send({ type: "member.remove", key: bob.key });
    expect((await bobClient.closing()).code).toBe(CLOSE.removed);
    expect(await ownerClient.expect("member.removed")).toMatchObject({ key: bob.key, by: alice.key });
    const back = await bob.connect(room);
    expect((await back.closing()).code).toBe(CLOSE.notAllowed);
  });

  it("does not let participants remove anyone", async () => {
    const alice = await Person.make("alice");
    const bob = await Person.make("bob");
    const { room, token } = await roomWithInvite(alice);
    const { client } = await bob.enter(room, { invite: token });
    client.send({ type: "member.remove", key: alice.key });
    expect(await client.expect("error")).toMatchObject({ reason: "only the owner can remove participants" });
  });

  it("lets a participant leave for good", async () => {
    const alice = await Person.make("alice");
    const bob = await Person.make("bob");
    const { room, ownerClient, token } = await roomWithInvite(alice);
    const { client } = await bob.enter(room, { invite: token });
    await ownerClient.expect("member.joined");
    client.send({ type: "room.leave" });
    expect((await client.closing()).code).toBe(1000);
    expect(await ownerClient.expect("member.left")).toMatchObject({ key: bob.key });
  });

  it("hands the room to the oldest participant when the owner leaves", async () => {
    const alice = await Person.make("alice");
    const bob = await Person.make("bob");
    const carol = await Person.make("carol");
    const { room, ownerClient, token } = await roomWithInvite(alice);
    const { client: bobClient } = await bob.enter(room, { invite: token });
    await new Promise((resolve) => setTimeout(resolve, 5));
    const { client: carolClient } = await carol.enter(room, { invite: token });
    ownerClient.send({ type: "room.leave" });
    expect(await carolClient.expect("owner.changed")).toMatchObject({ key: bob.key });
    bobClient.send({ type: "invite.list" });
    await bobClient.expect("invites");
    bobClient.send({ type: "member.remove", key: carol.key });
    expect(await bobClient.expect("member.removed")).toMatchObject({ key: carol.key });
  });

  it("deletes the room's data when the last participant leaves", async () => {
    const alice = await Person.make("alice");
    const room = roomId();
    const { client } = await alice.enter(room, { create: true });
    expect(await tables(room)).toEqual(expect.arrayContaining(["members", "invites", "schema"]));
    client.send({ type: "room.leave" });
    await client.closing();
    expect(await tables(room)).toEqual([]);
    // The id is free again.
    await (await Person.make("bob")).enter(room, { create: true });
  });
});

describe("persistence", () => {
  it("keeps participants and sockets through hibernation", async () => {
    const alice = await Person.make("alice");
    const { room, ownerClient, token } = await roomWithInvite(alice);
    await evictDurableObject(stubOf(room));
    ownerClient.send({ type: "invite.list" });
    expect((await ownerClient.expect("invites")).invites).toHaveLength(1);
    await (await Person.make("bob")).enter(room, { invite: token });
  });

  it("migrates a version 1 room to the current schema", async () => {
    const alice = await Person.make("alice");
    const room = roomId();
    await runInDurableObject(stubOf(room), (_instance, state) => {
      state.storage.sql.exec("CREATE TABLE schema (version INTEGER NOT NULL)");
      state.storage.sql.exec("INSERT INTO schema (version) VALUES (1)");
    });
    await alice.enter(room, { create: true });
    const version = await runInDurableObject(stubOf(room), (instance) => instance.schemaVersion());
    expect(version).toBe(SCHEMA_VERSION);
  });
});
