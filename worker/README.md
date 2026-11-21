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
