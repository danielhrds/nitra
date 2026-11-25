// Cameras: a participant shares a camera beside their screen, at most one
// of each, from any of their sockets.

import { describe, expect, it } from "vitest";

import { parse } from "../src/protocol";
import { Person, roomWithInvite } from "./helpers";

const CAMERA = { name: "Iriun Webcam", width: 1280, height: 720, fps: 30 };
const SCREEN = { kind: "window", name: "Valorant", width: 1920, height: 1080, fps: 60 };

describe("cameras", () => {
  it("are validated like screens and always of the camera kind", () => {
    expect(parse(JSON.stringify({ type: "camera.start", camera: CAMERA }))).toEqual({
      type: "camera.start",
      camera: { kind: "camera", ...CAMERA },
    });
    // Whatever kind is claimed, it is a camera.
    expect(parse(JSON.stringify({ type: "camera.start", camera: { ...CAMERA, kind: "desktop" } }))).toMatchObject({
      camera: { kind: "camera" },
    });
    expect(parse(JSON.stringify({ type: "camera.start", camera: null }))).toEqual({ error: "bad camera" });
    expect(parse(JSON.stringify({ type: "camera.start", camera: { ...CAMERA, fps: 0 } }))).toEqual({
      error: "bad camera",
    });
    // A screen cannot claim to be a camera.
    expect(parse(JSON.stringify({ type: "screen.start", screen: { ...SCREEN, kind: "camera" } }))).toEqual({
      error: "bad screen kind",
    });
  });

  it("show beside the screen, from another socket, and stop on their own", async () => {
    const [alice, bob] = await Promise.all([Person.make("alice"), Person.make("bob")]);
    const { room, ownerClient: a, token } = await roomWithInvite(alice);
    const screenSocket = (await bob.enter(room, { invite: token })).client;
    await a.expect("member.joined");
    await a.expect("presence.changed");
    screenSocket.send({ type: "screen.start", screen: SCREEN });
    await a.expect("presence.changed");
    // The camera comes from another socket of bob's (its own sharing thread).
    const cameraSocket = (await bob.enter(room)).client;
    cameraSocket.send({ type: "camera.start", camera: CAMERA });
    const both = await a.expect("presence.changed");
    expect(both).toMatchObject({ screen: { name: "Valorant" }, camera: { kind: "camera", name: "Iriun Webcam" } });
    expect((both.camera as { started_at: number }).started_at).toBeGreaterThan(0);
    cameraSocket.send({ type: "camera.stop" });
    expect(await a.expect("presence.changed")).toMatchObject({ screen: { name: "Valorant" }, camera: null });
  });

  it("go with the socket that shared it, and with leaving the room", async () => {
    const [alice, bob] = await Promise.all([Person.make("alice"), Person.make("bob")]);
    const { room, ownerClient: a, token } = await roomWithInvite(alice);
    const b = (await bob.enter(room, { invite: token })).client;
    await a.expect("member.joined");
    await a.expect("presence.changed");
    const camera = (await bob.enter(room)).client;
    camera.send({ type: "camera.start", camera: CAMERA });
    expect(await a.expect("presence.changed")).toMatchObject({ camera: { name: "Iriun Webcam" } });
    camera.ws.close(1000);
    await camera.closing();
    expect(await a.expect("presence.changed")).toMatchObject({ online: true, camera: null });
    // Leaving the room (room.exit) ends a camera too.
    b.send({ type: "camera.start", camera: CAMERA });
    expect(await a.expect("presence.changed")).toMatchObject({ entered: true, camera: { name: "Iriun Webcam" } });
    b.send({ type: "room.exit" });
    expect(await a.expect("presence.changed")).toMatchObject({ entered: false, camera: null });
  });
});
