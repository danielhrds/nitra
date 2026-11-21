// The Worker in front of the rooms: checks the request shape and hands
// room connections to that room's Durable Object. It holds no state.

import { ROOM_HEADER, Room } from "./room";

export { Room };

/** A room id: 16 random bytes in lowercase base32 (RFC 4648, no padding),
 * the same alphabet as the app's key codes. */
const ROOM_ID = /^[a-z2-7]{26}$/;
const ROOM_SOCKET = /^\/rooms\/([^/]+)\/ws$/;

export default {
  async fetch(request, env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/health") {
      if (request.method !== "GET") {
        return text(405, "method not allowed");
      }
      return Response.json({ ok: true });
    }
    const room = ROOM_SOCKET.exec(url.pathname);
    if (room) {
      const id = room[1] ?? "";
      if (!ROOM_ID.test(id)) {
        return text(400, "invalid room id");
      }
      if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
        return text(426, "expected a websocket upgrade");
      }
      // A fresh request, so the room id header is ours and never the
      // client's.
      const headers = new Headers(request.headers);
      headers.set(ROOM_HEADER, id);
      return env.ROOMS.get(env.ROOMS.idFromName(id)).fetch(new Request(request.url, { headers }));
    }
    return text(404, "not found");
  },
} satisfies ExportedHandler<Env>;

function text(status: number, body: string): Response {
  return new Response(body, { status });
}
