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

**Limits.** Only `auth` before authenticating, within 20 s (an alarm closes
late sockets); messages up to 64 KiB, text only; names cleaned and cut to 32
characters; at most 100 open invites per room.

| Close code | Meaning |
|---|---|
| 1000 | left the room |
| 1009 | message too big |
| 4001 | authentication failed (bad signature or talking before `auth`) |
| 4003 | not allowed (not a participant, invalid invite, room already exists) |
| 4004 | removed by the owner |
| 4008 | did not authenticate in time |

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
