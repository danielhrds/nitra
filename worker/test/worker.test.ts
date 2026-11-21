import { evictDurableObject, listDurableObjectIds, runInDurableObject } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

import { type Attachment, MAX_MESSAGE_BYTES, type Room, SCHEMA_VERSION } from "../src/room";

const BASE = "https://ziz.test";

/** A fresh valid room id (26 lowercase base32 characters). */
function roomId(): string {
  const alphabet = "abcdefghijklmnopqrstuvwxyz234567";
  const bytes = crypto.getRandomValues(new Uint8Array(26));
  return Array.from(bytes, (b) => alphabet[b % 32]).join("");
}

function get(path: string, init?: RequestInit): Promise<Response> {
  return exports.default.fetch(new Request(BASE + path, init));
}

/** A client WebSocket with a queue of what it receives. */
class Client {
  private messages: string[] = [];
  private waiters: (() => void)[] = [];
  closed: { code: number; reason: string } | undefined;

  constructor(readonly ws: WebSocket) {
    ws.accept();
    ws.addEventListener("message", (event) => {
      this.messages.push(event.data as string);
      this.wake();
    });
    ws.addEventListener("close", (event) => {
      this.closed = { code: event.code, reason: event.reason };
      this.wake();
    });
  }

  static async open(room: string): Promise<Client> {
    const response = await get(`/rooms/${room}/ws`, { headers: { Upgrade: "websocket" } });
    expect(response.status).toBe(101);
    if (!response.webSocket) {
      throw new Error("no websocket in the response");
    }
    return new Client(response.webSocket);
  }

  private wake(): void {
    for (const waiter of this.waiters.splice(0)) {
      waiter();
    }
  }

  private async until(ready: () => boolean): Promise<void> {
    const deadline = Date.now() + 2000;
    while (!ready()) {
      if (Date.now() > deadline) {
        throw new Error("timed out waiting for the socket");
      }
      await new Promise<void>((resolve) => {
        this.waiters.push(resolve);
        setTimeout(resolve, 50);
      });
    }
  }

  async next(): Promise<string> {
    await this.until(() => this.messages.length > 0);
    return this.messages.shift() as string;
  }

  async nextJson(): Promise<Record<string, unknown>> {
    return JSON.parse(await this.next());
  }

  async closing(): Promise<{ code: number; reason: string }> {
    await this.until(() => this.closed !== undefined);
    return this.closed as { code: number; reason: string };
  }

  send(message: string | object): void {
    this.ws.send(typeof message === "string" ? message : JSON.stringify(message));
  }
}

function stubOf(room: string): DurableObjectStub<Room> {
  return env.ROOMS.get(env.ROOMS.idFromName(room));
}

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
    for (const bad of ["abc", "A".repeat(26), "a".repeat(27), "0".repeat(26), "a".repeat(25) + "1"]) {
      expect((await get(`/rooms/${bad}/ws`, upgrade)).status, bad).toBe(400);
    }
  });

  it("asks for a websocket upgrade", async () => {
    expect((await get(`/rooms/${roomId()}/ws`)).status).toBe(426);
  });
});

describe("rooms", () => {
  it("welcome a participant", async () => {
    const client = await Client.open(roomId());
    client.send({ type: "hello" });
    expect(await client.nextJson()).toEqual({ type: "welcome", schema: SCHEMA_VERSION, connections: 1 });
  });

  it("put two connections to one room id in the same room, and other ids apart", async () => {
    const room = roomId();
    const before = (await listDurableObjectIds(env.ROOMS)).length;
    const first = await Client.open(room);
    const second = await Client.open(room);
    second.send({ type: "hello" });
    expect((await second.nextJson()).connections).toBe(2);
    const other = await Client.open(roomId());
    other.send({ type: "hello" });
    expect((await other.nextJson()).connections).toBe(1);
    expect((await listDurableObjectIds(env.ROOMS)).length).toBe(before + 2);
    first.ws.close();
  });

  it("answer keepalives without waking the room", async () => {
    const client = await Client.open(roomId());
    client.send("ping");
    expect(await client.next()).toBe("pong");
  });

  it("report messages that are not JSON or unknown", async () => {
    const client = await Client.open(roomId());
    client.send("{not json");
    expect(await client.nextJson()).toEqual({ type: "error", reason: "not JSON" });
    client.send({ type: "dance" });
    expect(await client.nextJson()).toEqual({ type: "error", reason: "unknown message" });
  });

  it("close a socket that sends a message too big", async () => {
    const client = await Client.open(roomId());
    client.send("x".repeat(MAX_MESSAGE_BYTES + 1));
    expect((await client.closing()).code).toBe(1009);
  });

  it("complete the closing handshake", async () => {
    const client = await Client.open(roomId());
    client.ws.close(1000, "bye");
    expect((await client.closing()).code).toBe(1000);
  });

  it("keep their SQLite schema across restarts, migrating once", async () => {
    const room = roomId();
    const stub = stubOf(room);
    const rows = () =>
      runInDurableObject(stub, (_instance, state) => state.storage.sql.exec("SELECT version FROM schema").toArray());
    expect(await rows()).toEqual([{ version: SCHEMA_VERSION }]);
    await evictDurableObject(stub);
    expect(await rows()).toEqual([{ version: SCHEMA_VERSION }]);
  });

  it("keep sockets and their attachments through hibernation", async () => {
    const room = roomId();
    const client = await Client.open(room);
    const stub = stubOf(room);
    await evictDurableObject(stub);
    client.send({ type: "hello" });
    expect(await client.nextJson()).toEqual({ type: "welcome", schema: SCHEMA_VERSION, connections: 1 });
    const attachments = await runInDurableObject(stub, (_instance, state) =>
      state.getWebSockets().map((ws) => ws.deserializeAttachment() as Attachment),
    );
    expect(attachments).toHaveLength(1);
    expect(attachments[0]?.connectedAt).toBeGreaterThan(0);
  });
});
