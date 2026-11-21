// Who is around in a room right now, from the participants' open sockets:
// online (the app is open), in the room (they entered), and the screen they
// share. Nothing here is stored: it lives in the sockets and goes with them.

import type { ScreenInfo } from "./protocol";

/** A screen being shared. */
export interface Screen extends ScreenInfo {
  started_at: number;
}

export interface Presence {
  online: boolean;
  entered: boolean;
  screen: Screen | null;
}

/** What one open socket says about its participant. */
export interface SocketState {
  key: string;
  entered: boolean;
  screen: Screen | null;
}

export const OFFLINE: Presence = { online: false, entered: false, screen: null };

/** A participant's presence over all their sockets: online with any, in the
 * room if any entered, and the (single) screen any of them shares. */
export function presenceOf(key: string, sockets: readonly SocketState[]): Presence {
  const mine = sockets.filter((s) => s.key === key);
  if (mine.length === 0) {
    return OFFLINE;
  }
  return {
    online: true,
    entered: mine.some((s) => s.entered),
    screen: mine.find((s) => s.screen !== null)?.screen ?? null,
  };
}

export function samePresence(a: Presence, b: Presence): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}
