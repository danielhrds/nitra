# ziz coordination (Cloudflare Worker)

The small backend that coordinates ziz rooms: who is in a room, which
screens are live, and the connection messages (offer/answer) between two
participants. **It never carries audio or video**: media goes directly
between the PCs. None of this is part of the desktop app.

One Durable Object per room keeps the room's small state in SQLite and the
participants' WebSockets, with the hibernation API so an idle room uses no
duration on the Free plan. Keepalives (`ping` → `pong`) are answered by the
runtime without waking the room.

Current routes:

| Route | What it does |
|---|---|
| `GET /health` | `{"ok":true}` |
| `GET /rooms/<id>/ws` | WebSocket to the room `<id>` (26 lowercase base32 characters, 16 random bytes) |

## Rooms

**Entering (no accounts, no passwords).** Each app has a device key
(Ed25519, kept with DPAPI). On connect the room sends
`{"type":"challenge","challenge":<hex of 32 random bytes>}`, new for every
socket. The app signs `"ziz room auth v1\0" + <room id> + "\0" + <challenge
bytes>` and answers:

```json
{"type":"auth","key":"<key code>","signature":"<hex>","name":"Ana",
 "create":true | "invite":"<token>"}
```

The room checks the signature with the public key. A participant gets in;
`create` on a room with nobody makes the first participant the **owner**;
a valid invite makes a new participant and uses one of its uses. Anything
else closes the socket. Getting in returns `welcome` (you and the
participants); the others get `member.joined`. The server stores public
keys only, and a signature is only good for that one socket.

**Invites.** Any participant sends `invite.create` (`ttl_secs`, default 1 h,
at most 30 days; `max_uses`, default 5, at most 100) and gets a 26-character
token. The room stores only its SHA-256 hash. `invite.list` shows the open
ones (no tokens). Only the owner sends `invite.revoke {id}`.

**Owner and leaving.** Only the owner sends `member.remove {key}`: that
participant's sockets close (4004) and they need a new invite to come back.
`room.leave` leaves for good; if the owner leaves, the oldest participant
becomes the owner (`owner.changed`); when the last participant leaves, the
room's data is deleted. Closing the app or losing the connection is not
leaving: participants stay in the room, which keeps existing with nobody
online, and they get straight back in the next time they open the app.

**Presence.** Not stored: it lives in the participants' open sockets and
goes with them. A participant with an open socket is **online**;
`room.enter` puts them **in the room** (`room.exit` takes them out, still a
participant); `screen.start` with `{kind: "desktop"|"window", name, width,
height, fps}` shares a screen (one per participant: a new one replaces the
old; it also enters the room) and `screen.stop` ends it. Every change goes
to everyone connected, in the room or not, as
`{"type":"presence.changed","key":…,"online":…,"entered":…,"screen":…|null}`,
and `welcome` carries everyone online (`presence`). A socket that closes
ends its screen and, if it was the last one, its participant's presence;
someone removed or leaving for good gets `member.removed`/`member.left`
instead. The app sends the `ping` keepalive about every 30 s; while anyone
is connected an alarm runs every 60 s and closes (4009) sockets with no
message or keepalive for 75 s, so a PC that vanished does not stay live.

**Signaling.** To watch a screen, the apps exchange an offer and an answer
(and a bye to stop) through the room; the media then goes directly between
the PCs. `{"type":"signal","to":<key code>,"envelope":<text>}` reaches
every open socket of that participant as
`{"type":"signal","from":<sender's key>,"envelope":<the same text>}`; `from`
is the sender's authenticated key, never anything the client sent. The
room does not read the envelope: it is the app's signed envelope (device
key, recipient, expiry, replay protection), checked by the receiving app,
so the room or Cloudflare could drop one but not forge or alter it. An
offline or unknown recipient, yourself, or more than 20 signals a minute
from one socket get `{"type":"signal.failed","to":…,"reason":…}`; an
envelope over 16 KiB is an `error`. Neither closes the socket.

**Limits.** Only `auth` before authenticating, within 20 s (an alarm closes
late sockets); messages up to 64 KiB, text only; names cleaned and cut to 32
characters, screen names to 64; screens 16 to 16384 pixels a side, 1 to 240
fps; at most 100 open invites per room.

| Close code | Meaning |
|---|---|
| 1000 | left the room |
| 1009 | message too big |
| 4001 | authentication failed (bad signature or talking before `auth`) |
| 4003 | not allowed (not a participant, invalid invite, room already exists) |
| 4004 | removed by the owner |
| 4008 | did not authenticate in time |
| 4009 | silent for 75 s (no message or keepalive) |

## Running it

Needs Node 22.12 or newer (Wrangler's requirement). Everything stays in this
folder: nothing is installed globally and no Cloudflare account is needed
for local work.

```powershell
npm install
npm run check   # type-check and tests (inside workerd, the Workers runtime)
npm run dev     # local server on http://127.0.0.1:8787
```

Set `WRANGLER_SEND_METRICS=false` to keep Wrangler from sending usage data
(`wrangler.jsonc` also sets `send_metrics: false`). `scripts/check.ps1`
runs `npm run check` when `node_modules` exists and the Node found (on
`PATH`, or in the folder named by `ZIZ_NODE_DIR`) is new enough; otherwise
it skips the worker.

Deploying comes later, after checking the Free plan limits.
