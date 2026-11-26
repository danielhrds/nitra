# nitra: benchmarks and measurements

**Read in:** English · [Português (Brasil)](benchmarks-pt-BR.md) · [Back to the README](../README.md)

## Benchmarks

Measured on a Ryzen 7 5800XT (16 threads) with an RX 7600, a 320 Hz main monitor and a 144 Hz second monitor. Game results come from Valorant's Practice Range in repeated, alternating rounds, compared with 95% confidence intervals; "no measurable difference" means the interval includes zero. CPU is a percentage of one core, with the share of the whole processor in parentheses where it matters. The methodology is in [`docs/benchmark-protocol.md`](benchmark-protocol.md).

### Impact on the game

Each stage of the share, added one at a time (1080p, 10 Mbps, uncapped, OBS closed):

| Condition | Average FPS | 1% low | Process CPU |
| --- | --- | --- | --- |
| No capture | 635.4 | 398.1 | — |
| Capture only | 601.2 (−5.4%) | 366.2 | 2.5–3.0% |
| Capture + GPU conversion | 600.6 (−5.5%) | 371.5 | 4.4–5.0% |
| Capture + conversion + AMF encode | 594.9 (−6.4%) | 359.7 | 7.7–8.7% |

Almost all of the cost is the capture itself: the game spends longer handing each frame to Windows while it is captured. Conversion and encode add no measurable difference to the average.

Windows Graphics Capture against Desktop Duplication, in the same session (OBS was most likely open, which lowers the baseline):

| Capture | Average FPS | 1% low | Capture process CPU |
| --- | --- | --- | --- |
| None | 569.7 | 342.7 | — |
| Windows Graphics Capture | 560.5 (−1.6%, no measurable difference) | 329.3 | 3.0% (0.19%) |
| Desktop Duplication | 558.0 (−2.1%, measurable) | 329.3 | 9.8% (0.61%) |

### Capture rate

| Frames asked from Windows Graphics Capture | Captured per second | Reach the encoder | Game FPS | Process CPU |
| --- | --- | --- | --- | --- |
| Twice the target rate | 114 | 59.9 fps | 597.4 | 7.9–9.5% |
| At the target rate | 63.4 | 57.6 fps | 603.4 (+1.0%) | 5.8–8.3% |

Asking only for the target rate is cheaper for the game but, on a 320 Hz monitor, lost frames to rounding. The final rule (90% of the shortest frame interval the monitor allows, with a credit-based limiter) keeps the lower rate and the full frame rate: dropped source frames went from 109–135 a minute to **0**, and the viewer gets a steady 59.9 fps.

### Sharing while playing

The full share (capture, conversion, encode and sending to a viewer on another PC) at 720p60, 6 Mbps:

| Scenario | Average FPS | 1% low | Cost |
| --- | --- | --- | --- |
| Uncapped, no OBS | 629 → 594 | 395 → 364 | −5.6% average, −7.8% 1% low |
| Uncapped, OBS replay buffer running | 540 → 522 | 302 → 265 | −3.3% average, −12% 1% low |
| **FPS capped at 300**, OBS replay buffer running | 299.8 → 299.6 | 223 → 218 | **no measurable difference** |

With the game capped, as most people play, the GPU has room to spare: the share only adds about 3% to the game's GPU time per frame and the frame rate does not move.

### Watching while playing

The viewer (hardware decode and presentation in a 1280×720 window on the second monitor) while playing on the main one:

| Stream | Uncapped: average FPS | Uncapped: 1% low | Capped at 300: 1% low |
| --- | --- | --- | --- |
| Nothing | 613.9 | 379.4 | 232.3 |
| 1080p30 | 610.9 (no measurable difference) | 369.8 (−2.5%) | 230.8 (no measurable difference) |
| 1080p60 | 608.4 (no measurable difference) | 365.3 (−3.7%) | 227.5 (no measurable difference) |
| 720p60 | 604.3 (−1.6%) | 360.4 (−5.0%) | 226.6 (−2.5%, borderline) |

Capped at 300, the average stayed at 299.8 in every case. The cost grows with the number of frames shown per second, not with the resolution.

### Alongside OBS

The same GPU encoder shared with an OBS replay buffer (AV1, 1080p60, 6.5 Mbps, high-quality preset), two minutes each, nitra sharing Valorant at 720p60 to a window on the second monitor:

| | OBS alone | OBS + nitra |
| --- | --- | --- |
| GPU video encoder load | 72.0% (peak 76.3%) | 60.6% (peak 61.0%) |
| GPU 3D load | 47.4% | 48.0% |
| OBS frames recorded / rendered | 9,091 / 9,102 | 10,424 / 10,436 |
| OBS encoding lag | none | none |
| nitra | — | 60 fps, 0 dropped, longest gap 59 ms |

The game with OBS recording, without and with nitra sharing (720p60, 6 Mbps):

| Scenario | Average FPS | 1% low | Game CPU / GPU time per frame |
| --- | --- | --- | --- |
| Uncapped, sharing the monitor | 539.6 → 521.5 (−3.35%) | 302.4 → 265 (−12.4%) | +3.2% / +4.2% |
| Uncapped, sharing the monitor (repeated) | 539.5 → 521.6 (−3.31%) | 299.8 → 261 (−12.8%) | — |
| Uncapped, sharing the game's window | 539.5 → 521.9 (−3.26%) | 299.8 → 267 (−11.1%), steadier | — |
| Capped at 300, sharing the window | 299.8 → 299.6 (no measurable difference) | 223.0 → 218 (no measurable difference) | GPU +2.8% |

Capturing the game's window or its whole monitor costs the same; the window gave slightly steadier lows.

### Bitrate: 6 Mbps or 2 Mbps

720p60 at 6 Mbps (the default) against 720p30 at 2 Mbps (the economy preset), on a lossless Practice Range clip:

| Setting | VMAF | VMAF, worst 5% | Sender CPU | Viewer CPU | Game FPS cost |
| --- | --- | --- | --- | --- | --- |
| 720p60 · 6 Mbps | **94.4** | 86.8 | 7.4% (0.46%) | 3.5–4.2% (~0.24%) | −5.6% |
| 720p60 · 3 Mbps | 82.5 | 67.3 | — | — | — |
| 720p60 · 2 Mbps | 70.5 | 48.4 | — | — | — |
| 720p30 · 2 Mbps | 84.2 | 69.3 | 3.3% (0.21%) | 1.6–2.2% (~0.12%) | −4.9% |
| 720p30 · 1.5 Mbps | 76.5 | 57.4 | — | — | — |

VMAF scores how close the picture is to the original (above ~93 looks the same). At a low bitrate, 30 fps keeps each frame sharper than 60 fps; the game pays about the same either way, so the default stays at 6 Mbps and the 2 Mbps preset is there for slow uploads. Memory is the same in both. On the wire, packet headers add about 1.5% to the bitrate.

### Encoder

| Encoder | Latency p50 / p99, 1080p 10 Mbps | Latency p50 / p99, 1080p 20 Mbps | CPU to submit a frame |
| --- | --- | --- | --- |
| AMF | **2.3 / 2.9 ms** | 2.3 / 2.6 ms | 177 µs |
| Media Foundation | 5.9 / 6.2 ms | 5.9 / 6.0 ms | 281 µs |

Both hit the target bitrate (within 1%) with no dropped frame. Quality on a game clip at the same bitrate (Media Foundation / AMF):

| Output | PSNR-Y (dB) | SSIM | VMAF |
| --- | --- | --- | --- |
| 1080p · 6 Mbps | 36.2 / 36.2 | 0.951 / 0.950 | 82.8 / 82.2 |
| 1080p · 10 Mbps | 39.0 / 38.9 | 0.967 / 0.967 | 92.5 / 92.0 |
| 1080p · 20 Mbps | 42.8 / 42.6 | 0.982 / 0.981 | 98.4 / 98.2 |
| 720p · 6 Mbps | 39.6 / 39.5 | 0.969 / 0.969 | 94.7 / 94.4 |

The two encoders run on the same hardware and give the same quality; AMF wins on latency and avoids Media Foundation's intermittent slow mode (about 30 ms per frame). Capture and encode together in the game, 1080p 10 Mbps:

| OBS | AMF | Media Foundation |
| --- | --- | --- |
| Replay buffer running | −2.9% average FPS | −4.6% |
| Closed | −7.0% | −7.4% |

### Conversion and decode

| Step | Result |
| --- | --- |
| BGRA → NV12 shader, 1080p | 36.6 µs of GPU per frame (0.22% of the GPU at 60 fps) |
| BGRA → NV12 shader, 720p scaled | 23.4 µs (0.14%) |
| Hardware decode, 1080p 10 Mbps | 234 µs p50, 476 µs p99 per frame; up to about 930 frames per second |
| Hardware decode, 720p 6 Mbps | 207 µs p50 |

### End to end

From the moment a frame is rendered to the moment it reaches the display, on the same PC (1080p 10 Mbps, measured with PresentMon at the display):

| Encoder | Render → on screen, p50 | p95 | Decoded → on screen, p50 |
| --- | --- | --- | --- |
| AMF | **10.3–11.2 ms** | 23–24 ms | 3.3 ms |
| Media Foundation | 13.4–14.7 ms | 26–27 ms | 3.4 ms |

Between two processes on the same PC, render → decoded is 10.2 ms p50 against 9.7 ms inside one process: the network path adds under 1 ms.

### Host cost per viewer (1080p60, 10 Mbps)

| Viewers | Host CPU | Video encoder load | Render → decoded, p50 |
| --- | --- | --- | --- |
| 1 | 5–6% (0.3–0.4%) | 9.5% | 4.8 ms |
| 3 | 13–15% (0.8–0.9%) | 9.5% | 5.7 ms |
| 5 | 21–23% (1.3–1.4%) | 9.3% | 7.3 ms |

The encode runs once for everyone. Each viewer adds about 4% of one core, roughly 0.25% of the whole processor, almost all of it in the system call that sends each packet.

### Network

Recovery on an impaired network (1080p60, 10 Mbps, two runs):

| Condition | Frames decoded | Longest freeze |
| --- | --- | --- |
| 1% loss | 97–100% | 34–84 ms |
| 5% loss | 83–87% | 1.5–2.4 s |
| 10% loss | 46–65% | 2.7–4.3 s |
| Loss bursts | 96–99% | 0.2–0.65 s |
| 2 s outage | 100% | back about 40 ms after the network returns |
| 50 ± 20 ms delay, 2% reordering | 98–100% | in order |

No frame arrived corrupted in any condition. Packet loss shown in the app matches the simulated loss (5.36% seen for 5% simulated). These runs predate the wider retransmission window; with it, 2–5% loss no longer freezes the picture (see [Packet loss](#packet-loss) below).

### Audio

| Measure | Result |
| --- | --- |
| Sound to ear on the viewer | 56.5 ms p50 (61 ms p95) |
| Audio against the picture | 33 ms after it |
| Sound to decoded, 5% packet loss | 7 ms p50, 48–67 ms p95 |
| Back in sync after a 2 s outage | at once (one gap) |
| Opus encode / decode, per 20 ms block | 166 µs / 42 µs |
| Player over 10 minutes | 0 underruns, 1.9% of one core, 3.3 MB |
| Each app captured | about 0.15% of one core (0.01%), 0.1 MB |

### The viewer

The viewer's cost by stream, out of the game (decode runs on the GPU's separate video engine):

| Stream | GPU 3D | GPU video decode | CPU | Video memory |
| --- | --- | --- | --- | --- |
| 1080p60 | 1.0% | 12.9% | 0.3–5.6% | 112 MB |
| 1080p30 | 0.5% | 7.2% | 0.3–1.1% | 112 MB |
| 720p60 | 0.5% | 7.0% | 2.3–3.5% | 57 MB |
| 720p30 | 0.3% | 3.7% | 0.3–1.2% | 57 MB |

Memory per watched screen, before and after sharing one video device:

| | Before | After |
| --- | --- | --- |
| One screen | 137 MB | 106 MB |
| Two screens | 217 MB | 159 MB |
| Each extra screen | +80 MB, +699 handles, +26 threads | +53 MB, +105 handles, +10 threads |

Declaring one reference frame instead of four (same stream otherwise: the two files differ by 2 bytes, and VMAF, PSNR and latency are identical), a viewer with its window, alternating rounds:

| Stream | Video memory | Commit | Task Manager "Memory" | Smoothness |
| --- | --- | --- | --- | --- |
| 720p60, 6 Mbps | 58.8 → **49.8 MB** | 76.6 → 71.8 MB | 25.5 MB, unchanged | every decoded frame shown, longest gap 33–34 ms in both |
| 1080p60, 10 Mbps | 113.8 → **95.6 MB** | 111.3 → 102.3 MB | 25.9 MB, unchanged | every decoded frame shown, longest gap 34–35 ms in both |

Task Manager's "Memory" column is the private working set: memory actually in use. The commit also counts memory the driver reserves and mostly never touches, so it is larger. Watching one 720p screen in the app takes it from 24 to 33 MB in Task Manager (commit 42 → 95 MB, video memory 20 → 68 MB).

CPU stayed the same (6.3% of one core with two screens, in alternating rounds), with 0 dropped frames. The window shown through the desktop compositor costs less than one bound to the window: 2.1% of one core instead of 2.9% while watching, with no change in the game (−0.09%).

### The app

| Scenario | Result |
| --- | --- |
| Idle: in or out of a room, minimized or in the tray | 0% CPU, 56 MB |
| Window start | about 106 ms |
| Sharing 720p60 | 2.3–4.3% of one core (0.15–0.27%), 133 MB |
| Sharing and watching at the same time | 8–11% of one core (0.5–0.7%), 217 MB |
| Close while sharing and watching | 0.3–0.4 s |
| Second launch of the same profile | brings the open window forward in 77 ms |
| 10 start/stop cycles of sharing or watching | memory and handles back to the start: no leak |
| Each saved room or friend, idle | 0% CPU, about 1.3 MB (11 rooms: +14 MB) |
| Switching what is shared | 60–160 ms on the host; the viewer stays connected, with its longest gap 183–264 ms (46–49 ms without a switch) and no frame dropped |
| Preview of your own share | about +0.5% of one core (within the noise), +7 MB of memory and +5 MB of video memory; drawn by its own video window, so the interface does not redraw for it |
| Floating window | no extra cost: the same video window, moved above the others |

Against 0.1.0-beta.2, built from its tag and run alternately in the same scenarios (3 rounds of 60 s), nothing grew: the idle window and sharing 1080p60 to a viewer use the same memory (36, 206 and 114 MB) and the same CPU within the noise of one round to the next (host 10.1% vs 10.8% of one core on average, viewer 5.8% vs 4.5%).

### Camera

| Scenario | Result |
| --- | --- |
| Capture, Iriun Webcam 720p30 (YUY2) | 30 frames/s, none dropped; 0.9–1.9% of one core (0.06–0.12%); 0.19 ms per frame (median) |
| Capture, OBS Virtual Camera (offers only 1080p60, kept at 30) | ours 0.94% of one core; the OBS filter itself, which runs inside nitra, 9% (drawing its waiting screen) |
| Sharing a camera (capture, GPU conversion, encode, send), Iriun | 1.5% of one core (0.09%), 87 MB |
| Watching a camera | 3.2–3.4% of one core (0.2%), 113–121 MB |
| Watching a screen and a camera together | 4.9–6.0% of one core (0.31–0.37%), 149 MB: less than the two apart, which share the window's own work |
| Turning the camera off | back to 0% CPU and to the memory before it was on |

A camera hands its frames over in memory, so each frame is copied to the GPU once and converted there (YUY2, NV12, RGB24 and RGB32), then encoded like a screen. A camera's own pacing is trusted: the frame-rate limiter only thins one faster than asked for, since frames that come over a network (the Iriun) arrive in bunches the limiter would cut.

### Rooms server

| Measure | Result |
| --- | --- |
| Sign in on Cloudflare, room starting cold | 0.91 s |
| Offer and answer over the internet | 330 ms |
| Someone leaves without warning | seen by the others in 160 ms |
| A quiet room | hibernates; wakes twice a minute while someone is connected |
| Encrypted preview | 1–3 ms to make and seal; up to 22 KB for two members |
| Production, end to end | 479–482 frames in 8 s through the deployed Worker |

## Measurements

These checks look at what a viewer sees: whether frames arrive, how evenly they reach the screen, and how the stream holds up on a bad network. Same PC as above; host and viewer on one PC over loopback unless said otherwise, the network trouble simulated on the viewer's side with a fixed seed so runs can be compared.

### What the app records

While sharing or watching, nitra writes one line to its local log every 10 seconds (about 110 KB an hour per watched screen; nothing leaves the PC). The card's **⋯** button shows the same numbers over the video.

| Side | Numbers |
| --- | --- |
| Sharing | frames captured, encoded and sent per second; frames lost before encode or before sending; bitrate |
| Watching | frames received, decoded and shown per second; frames dropped and replaced; spacing on screen (median, 95th percentile, longest) and the share of long frames (over 1.5× the median); resolution, Mbps, loss, round trip |
| Watching, sound | how long sound takes from arriving to being heard (queue, sound card buffer and the device's own latency); catch-ups, concealed packets, gaps, underruns |

The card's frame rate counts frames shown, not frames decoded. Spacing on screen comes from the swap chain's frame-latency signal: the frame statistics a windowed swap chain gets from DXGI follow the main display, matching about 90% of frames there and about 1% on a second monitor at another refresh rate. Against them, on the main monitor, the signal gives the same median and 95th percentile (16.3 vs 15.6 ms, 33.2 vs 34.4 ms).

### Frames from a real game

Valorant's window shared at 1080p60, 10 Mbps, to a viewer decoding on the same PC (320 Hz monitor), 120 s:

| Stage | Result |
| --- | --- |
| Captured, encoded, sent | 59.6–60.2 fps in every 10 s line, nothing lost |
| Received, decoded | 59.8–60.2 fps, 7,207 frames, 0 dropped |
| Longest gap | 81 ms, once |

The synthetic test window used elsewhere does not reach its own screen at an even 60 fps on a 320 Hz monitor (7.7% long frames before any capture); the capture delivers what is on screen, with no frame dropped by the limiter.

### Sound over a long session

30 minutes of synthetic picture and sound, the viewer playing:

| Measure | Result |
| --- | --- |
| Arrival to heard | 45.0–46.1 ms from start to end |
| Sound against the picture | 34.7 ms after it |
| Catch-ups, concealed packets, gaps | 0 |
| Underruns | 1, at the start |
| Sound card clock drift | +48 ppm, compensated |

The player never holds more than its 60 ms target plus 40 ms: any more is dropped at once, so the delay cannot grow with time.

### Packet loss

1080p60, 10 Mbps, 60 s per run, before and after the wider retransmission window (two runs at each loss rate):

| Condition | Frames skipped | Keyframe requests | Longest freeze | Shown fps |
| --- | --- | --- | --- | --- |
| No trouble | 0 | 1 | 34–42 ms | 58–60 |
| 2% loss, before | 78–194 | 8–15 | 0.15–1.5 s | 46–57 |
| **2% loss, after** | **0** | **1** | **35–192 ms** | 48–57 |
| 5% loss, before | 466–505 | 35–38 | 1.1–1.6 s | 26–52 |
| **5% loss, after** | **0–2** | **1–2** | **35–68 ms** | 48–52 |
| Loss bursts (10 packets, ~1.7%) | 3 → 0 | 2 → 1 | 83 → 59 ms | 56–58 |
| 2 s outage | 6–25 | 2 | the outage itself | back at once |

A viewer that only receives has no round trip from RTP, so str0m retries a lost packet every 105 ms, and it tracked only the last 100 packets (about 110 ms at 10 Mbps): a lost request or a lost retransmission fell out of the window before the retry, and became a keyframe request and a freeze. nitra carries str0m with that window raised to 1,000 packets ([`vendor/str0m`](../vendor/str0m/NITRA.md)) and lets video wait for retransmissions over as many. Frames that waited still arrive together, so 24–44% of frames stay long on screen at 2–5% loss.

### Smoothing

Each viewer picks how much delay to trade for smoothness (Settings → "When watching"); the default is automatic. Frames are shown at their capture time plus a short hold, instead of as soon as they arrive. 1080p60, 10 Mbps, 60 s per run:

| Condition | Mode | Long frames | Spacing p95 | Frames shown | Hold (added delay) |
| --- | --- | --- | --- | --- | --- |
| No trouble | Minimum delay | 10–17% | 31–34 ms | 3,672 | 0 |
| No trouble | Smooth | 7–10% | 29–31 ms | 3,673 | 31 ms |
| No trouble | Automatic | 9–13% | 31 ms | 3,653 | 10–16 ms |
| 2% loss | Minimum delay | 24–29% | 47–48 ms | 3,413 | 0 |
| 2% loss | Smooth | 13–16% | 31 ms | 3,655 | 31 ms |
| 2% loss | Automatic | 12–15% | 31 ms | 3,648 | 28–31 ms |
| 5% loss | Minimum delay | 40–42% | 50–60 ms | 3,166 | 0 |
| 5% loss | Smooth | 13–16% | 32–34 ms | 3,605 | 31 ms |
| 5% loss | Automatic | 15–19% | 31–36 ms | 3,544 | 31 ms |

The hold is at most two frames (~33 ms at 60 fps); automatic raises it at once to cover 95% of recent arrivals and lowers it about 2 ms a second. The test window's own unevenness (see above) is why it holds 10–16 ms even with no network trouble. The viewer's CPU was the same in every mode (2.8–3.6 s of one core per minute, about 5%); up to three frames wait in GPU textures, about 6 MB more video memory per watched screen at 1080p.

### A link slower than the stream

| Link (10 Mbps stream) | Frames shown in 60 s | Longest freeze | Keyframe requests | Encoder changes |
| --- | --- | --- | --- | --- |
| 6 Mbps, encoder at 100% of the estimate | 1,699–2,201 | 1.9–3.3 s | 41–75 | 53–74 |
| **6 Mbps, at 85%, rising in steps** | **3,250** | **0.99 s** | **13** | 26 |
| 3 Mbps, encoder at 100% of the estimate | 1,606–1,917 | 3.1–3.2 s | 63–68 | 38–79 |
| **3 Mbps, at 85%, rising in steps** | **2,761** | **1.1 s** | **31** | 35 |

At 100% of the bandwidth estimate (the pacer sends about 10% above it, and keyframes come in bursts) the bottleneck's queue overflowed, keyframes overflowed it again and the viewer kept asking for new ones. The encoder now gets 85% of the estimate, follows a drop at once and rises only after 2 s, by at most 15% a step: on 6 Mbps the picture holds 58–60 fps between drops. After a deep drop the climb back takes about half a minute. On good links nothing changes (10 Mbps, no encoder change, in the same runs).
