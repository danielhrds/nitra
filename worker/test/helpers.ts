import { exports } from "cloudflare:workers";
import { expect } from "vitest";

import { authMessage, base32Encode, hexDecode, hexEncode, randomBytes } from "../src/crypto";

export const BASE = "https://ziz.test";

/** A fresh valid room id: 16 random bytes in base32, as the app makes it. */
export function roomId(): string {
  return base32Encode(randomBytes(16));
}

export function get(path: string, init?: RequestInit): Promise<Response> {
  return exports.default.fetch(new Request(BASE + path, init));
}

export type Json = Record<string, unknown>;

/** A client WebSocket with a queue of what it receives. */
export class Client {
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

  private async until(ready: () => boolean, what: string): Promise<void> {
    const deadline = Date.now() + 2000;
    while (!ready()) {
      if (Date.now() > deadline) {
        throw new Error(`timed out waiting for ${what}`);
      }
      await new Promise<void>((resolve) => {
        this.waiters.push(resolve);
        setTimeout(resolve, 50);
      });
    }
  }

  async next(): Promise<string> {
    await this.until(() => this.messages.length > 0 || this.closed !== undefined, "a message");
    const message = this.messages.shift();
    if (message === undefined) {
      throw new Error(`closed instead: ${JSON.stringify(this.closed)}`);
    }
    return message;
  }

  async nextJson(): Promise<Json> {
    return JSON.parse(await this.next());
  }

  /** The next message of `type`, skipping others. */
  async expect(type: string): Promise<Json> {
    for (;;) {
      const message = await this.nextJson();
      if (message.type === type) {
        return message;
      }
    }
  }

  async closing(): Promise<{ code: number; reason: string }> {
    await this.until(() => this.closed !== undefined, "the close");
    return this.closed as { code: number; reason: string };
  }

  /** Whether anything arrived (or the socket closed) in a short while. */
  async quiet(ms = 150): Promise<boolean> {
    await new Promise((resolve) => setTimeout(resolve, ms));
    return this.messages.length === 0 && this.closed === undefined;
  }

  send(message: string | object): void {
    this.ws.send(typeof message === "string" ? message : JSON.stringify(message));
  }
}

/** Someone with a device key, like the app's identity. */
export class Person {
  private constructor(
    readonly name: string,
    private readonly keys: CryptoKeyPair,
    readonly key: string,
  ) {}

  static async make(name: string): Promise<Person> {
    const keys = (await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"])) as CryptoKeyPair;
    const raw = new Uint8Array((await crypto.subtle.exportKey("raw", keys.publicKey)) as ArrayBuffer);
    return new Person(name, keys, base32Encode(raw));
  }

  async sign(message: Uint8Array): Promise<string> {
    return hexEncode(new Uint8Array(await crypto.subtle.sign({ name: "Ed25519" }, this.keys.privateKey, message)));
  }

  /** Connects and answers the room's challenge; returns the socket and the
   * challenge it signed. */
  async connect(room: string, extra: Json = {}): Promise<Client> {
    const client = await Client.open(room);
    const challenge = await client.expect("challenge");
    const signature = await this.sign(authMessage(room, hexDecode(challenge.challenge as string) as Uint8Array));
    const roomName = extra.create === true && extra.room_name === undefined ? { room_name: "Test room" } : {};
    client.send({ type: "auth", key: this.key, signature, name: this.name, ...roomName, ...extra });
    return client;
  }

  /** Connects and expects to get in. */
  async enter(room: string, extra: Json = {}): Promise<{ client: Client; welcome: Json }> {
    const client = await this.connect(room, extra);
    const welcome = await client.nextJson();
    expect(welcome.type, JSON.stringify(welcome)).toBe("welcome");
    return { client, welcome };
  }
}

/** A room created by `owner`, with an invite token. */
export async function roomWithInvite(
  owner: Person,
  invite: Json = {},
): Promise<{ room: string; ownerClient: Client; token: string; inviteId: string }> {
  const room = roomId();
  const { client } = await owner.enter(room, { create: true });
  client.send({ type: "invite.create", ...invite });
  const created = await client.expect("invite.created");
  return { room, ownerClient: client, token: created.token as string, inviteId: created.id as string };
}
