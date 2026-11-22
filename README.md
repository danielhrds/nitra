<p align="center"><img src="docs/logo.png" width="128" alt="ziz logo"></p>

# ziz

Low-latency screen sharing for Windows, built in Rust for a few friends at a time. You share a game or a window in a room; up to three friends watch it live, peer to peer, while the game keeps running smoothly.

> **Status:** private beta. Validated on an AMD Radeon RX 7600 with Windows 11. NVIDIA and Intel are experimental (see [Hardware](#hardware)).

## Features

- **Rooms, not calls.** Create a room, send the invite, and everyone in it sees who is live. Several people can share at once, and you can watch more than one screen side by side, focus one, or go full screen.
- **Share a screen or a single window**, with its own sound: a window shares only its app's audio; a whole screen shares the system sound minus Discord, ziz and any app you exclude.
- **Quality presets:** 720p60 (the default), 1080p60, 1080p30 and 1440p60, plus an economy 720p30 at 2 Mbps for slow uploads, with an advanced bitrate setting. The picture keeps its proportions when the source changes shape.
- **Encrypted previews** of each live screen for people in the room who are not watching.
- **Live numbers** on every watched screen: frame rate, bitrate, packet loss, round trip and the connection route (LAN, internet, IPv6). When a direct connection fails, the app says why.
- **Tray icon**, one window per profile, and clear messages when a shared window closes, a monitor goes away or the PC sleeps.
- **Recovers by itself** from a lost GPU (driver reset), sleep and display changes.

## Install, use and remove

1. Download `ziz.exe`. It is a single file; there is no installer.
2. Run it. Windows may show "Windows protected your PC" because the file is not signed: click **More info → Run anyway**.
3. Choose your name, create a room and copy the invite to your friends.

ziz keeps its data in one folder, `%LOCALAPPDATA%\ziz`: the device key (protected by Windows DPAPI), your rooms, your settings and the logs. It writes nothing else: no registry keys, temporary files or cache.

**To remove ziz,** delete `ziz.exe` and that folder. Windows itself may keep a firewall rule if you allowed ziz through the firewall; it is harmless.

## Hardware

| GPU | Sharing (encode) | Watching (decode) |
| --- | --- | --- |
| AMD | **Tested** (AMF, RX 7600) | **Tested** |
| NVIDIA | Experimental (Media Foundation / NVENC) | Windows H.264 decoder (DXVA) |
| Intel | Experimental (Media Foundation / Quick Sync) | Windows H.264 decoder (DXVA) |

On a GPU that has not been tested, ziz asks before the first share. ziz never encodes in software, because that would weigh on the game; without a hardware H.264 encoder it says so and suggests updating the driver.

## Known limitations

- **Laptops with two GPUs:** ziz uses the first one, which may be the integrated GPU while the game runs on the other.
- **No relay server:** when strict NAT or a firewall blocks a direct connection on both sides, watching does not start. The app tells you which side is the likely cause.
- Not yet stress-tested over multi-hour sessions or hundreds of start/stop cycles.

## How it works

```text
capture (Windows Graphics Capture)
  -> convert and scale on the GPU (BGRA -> NV12, keeps the aspect ratio)
  -> one hardware H.264 encoder (AMF on AMD, Media Foundation elsewhere)
  -> WebRTC to each viewer (DTLS-SRTP), congestion control per viewer
  -> hardware decoder -> D3D11 texture -> the app's window
```

Frames stay on the GPU from capture to encoder; no frame pixels go through CPU memory. The rooms server, a Cloudflare Worker with one Durable Object per room, only carries sign-in, presence, signaling and sealed previews. The media never passes through it.

### Key decisions

- **Windows Graphics Capture** over Desktop Duplication: the same game impact within noise, at a third of the CPU.
- **GPU shader** for color conversion and scaling.
- **AMF** as the AMD encoder, with Media Foundation as the fallback and the path on other GPUs.
- **Dear ImGui** for the interface: it sleeps when nothing changes, while GPUI woke about 650 times a second and cost 4% of the game's 1% low.
- **Peer to peer, 1–3 viewers,** with no media server.

## Performance design

The goal is a game that stays smooth while it is shared, not the lowest number in isolation. Every choice below was measured, A/B, against the alternative.

**Capture and encode**
- **GPU only, from capture to encoder.** Frames stay as D3D11 textures; color conversion and scaling run in one shader pass into a fixed pool of NV12 textures. No frame pixels go through CPU memory.
- **Capture only what is sent.** Windows Graphics Capture is asked for frames at the target rate, timed from the captured monitor's refresh rate, instead of every frame the game draws (about 114 per second at 60 fps). That lowers the capture's CPU and its cost to the game.
- **A steady 60 fps.** A credit-based rate limiter absorbs the bursts in a game's frame delivery, so no source frame is dropped and viewers get a steady ~59.9 fps.
- **AMF on AMD.** It encodes in 2.3 ms against about 5.4 ms for Media Foundation, and it avoids Media Foundation's intermittent slow mode.
- **Asynchronous encode with bounded queues.** At most 4 frames are in flight in the encoder; every queue has a fixed size, so nothing grows without limit under load.
- **Instant picture for late joiners.** When the source is minimized or still, the last frame is encoded again on request, so a new viewer never waits on a black screen.

**Network**
- **One encode for everyone.** Each viewer only adds the cost of sending packets: about 4% of one core, roughly 0.25% of the whole processor.
- **Slow links do not hold back the others.** Each viewer has a send budget of 300 ms; a viewer on a slow link skips frames and resumes at the next keyframe, while the encoder follows the slowest viewer that keeps up. Keyframes are limited to one per 500 ms, however many viewers ask.
- **Audio that catches up.** Opus with forward error correction and loss concealment. Late audio is capped, so with 5% packet loss sound reaches the viewer in 48–67 ms (p95), and audio resumes about 50 ms after a 2-second network drop.
- **Per-app audio capture.** Each app captured costs about 0.15% of one core (0.01% of the whole CPU) and 0.1 MB.

**Watching**
- **One video device for every watched screen.** The decoders run on the window's own D3D11 device instead of one each, so a watched screen costs about 50 MB rather than about 80 MB, with no extra CPU.
- **Hidden screens cost nothing.** A watched screen that is minimized or covered skips drawing: 0% 3D load, 40% fewer wake-ups and 14 MB less video memory.
- **Nothing runs when nothing changes.** The window sleeps until there is input or a new frame: 0% CPU while idle, and it opens in about 106 ms.

**Rooms server**
- **Rooms sleep between messages.** Each room is a Durable Object with hibernating WebSockets; keepalive pings are answered by the runtime without waking the room, so a quiet room costs one request a minute.

## Benchmarks

Measured on a 16-thread PC with an RX 7600 and a 320 Hz monitor. Game results come from Valorant's Practice Range in repeated, alternating rounds, with 95% confidence intervals against a no-capture baseline. CPU is a percentage of one core; divide by 16 for the share of the whole processor. The methodology is in [`docs/benchmark-protocol.md`](docs/benchmark-protocol.md).

### Impact on the game

| Condition | Average FPS | 1% low | Capture process CPU |
| --- | --- | --- | --- |
| No capture | 569.7 | 342.7 | — |
| Windows Graphics Capture | 560.5 (−1.6%, within noise) | 329.3 | 3.0% (0.19% total) |
| Desktop Duplication | 558.0 (−2.1%, measurable) | 329.3 | 9.8% (0.61% total) |

Sharing the game's monitor in full (capture, conversion, encode and sending to a viewer) costs about 5% of the average FPS: 594–598 FPS against 629 FPS without ziz, almost all of it from capture. The bitrate barely matters: 720p60 at 6 Mbps cost 5.6% and 720p30 at 2 Mbps cost 4.9%, a gap within the measurement's confidence intervals.

### Encoder

| Encoder (1080p60) | Encode latency p50 / p99 |
| --- | --- |
| AMF | 2.3 / 3.1 ms |
| Media Foundation | about 5.4 ms p50 |

### End to end

| Path (1080p60, same PC) | Render → on screen, p50 |
| --- | --- |
| AMF | 13.9–15.4 ms |
| Media Foundation | 17.0–21.2 ms |

Measured with PresentMon at the moment the frame reaches the display, not when the app hands it over.

### Host cost per viewer (1080p60, 10 Mbps)

| Viewers | Host CPU | Video encoder load | Render → decoded, p50 |
| --- | --- | --- | --- |
| 1 | 5–6% (0.3–0.4% total) | 9.5% | 4.8 ms |
| 3 | 13–15% (0.8–0.9% total) | 9.5% | 5.7 ms |
| 5 | 21–23% (1.3–1.4% total) | 9.3% | 7.3 ms |

The encode runs once for everyone. Each viewer adds about 4% of one core, roughly 0.25% of the whole processor, mostly in sending packets.

### The app

| Scenario | Result |
| --- | --- |
| Window idle | 0% CPU |
| Window start | about 106 ms |
| Sharing + watching on the same PC | about 8–11% of one core (0.5–0.7% of the whole CPU) |
| Memory per watched screen | about 50 MB |
| Dropped source frames at 60 fps (60 s) | 0 |
| Viewer frame rate, steady | about 59.9 fps |

## Security

- **No accounts or passwords.** Each device has a key pair; the private key is protected by Windows DPAPI. Rooms sign you in by challenge–response with that key.
- **Signed messages** with expiry and replay protection. Manual invites work once; room invites are stored only as hashes, with an expiry and a use limit.
- **Encrypted media**, peer to peer (DTLS-SRTP). Previews are sealed for each room member, and the server cannot open them.
- **No IP addresses in the logs.** Connection details are logged as kinds and routes only, and the Worker keeps no request logs.
- **Hardened against malformed input:** fuzz tests on every parser of outside data (room protocol, invites, signed envelopes, STUN, profile files, previews). Previews accept JPEG only. Oversized datagrams are dropped instead of ending the session. STUN transaction ids come from the system's cryptographic random generator.
- **Dependencies audited** with `cargo audit`: 0 known vulnerabilities.
- **GPU safety:** only documented user-mode APIs. ziz never touches clocks, voltages, power limits, drivers or the TDR settings.

## Building

Requirements: Windows 10 or 11, the Rust toolchain pinned in `rust-toolchain.toml`, and libopus (through vcpkg).

```powershell
cargo build --release -p ziz
.\scripts\check.ps1
```

`check.ps1` runs formatting, clippy and the test suite. The rooms Worker lives in `worker/` (TypeScript, Wrangler).
