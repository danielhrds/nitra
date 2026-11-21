# Game impact benchmark protocol

How to measure a game's frame pacing reproducibly, first without ziz (the
baseline) and later with ziz running, so the two can be compared honestly.

Raw captures and reports go to the `lab` folder next to the repository
(`ZIZ_LAB_DIR` overrides it). They are never committed.

## Tools

- **PresentMon** (console build, GameTechDev, official GitHub releases). It
  reads Windows ETW events and injects nothing into the game. It needs
  administrator rights. Version used for the first baseline: 2.6.0.
- **`bench frames`** computes the metrics below from PresentMon CSV files.
- **`scripts/bench-baseline.ps1`** runs the whole procedure.

## Scene

The default scene is **VALORANT, The Range**: no other players, no rounds,
always the same place.

- Stand on a fixed spot and look at a fixed point. Use the same spot and view
  in every session you want to compare.
- The view must not change during a capture: no mouse or keyboard input.
- The Range kicks the player after about 3.5 minutes without any input. Tap
  crouch (Ctrl) once in each pause between captures; the camera returns to the
  same place before the next capture starts.
- Keep the game focused for the whole session. A game in the background runs
  at a different frame rate.

## Environment

Record it with every session (the script does this automatically, read only):

- Windows build, GPU driver, monitors and refresh rates (`bench probe`).
- Power plan, hardware-accelerated GPU scheduling, Game Mode.
- PresentMon version and SHA-256.
- In-game settings that matter: resolution, display mode, FPS limit, VSync.
  The FPS limit is not reliably readable from the game files (they hold
  profiles of several accounts); write it in `-Notes`.

Nothing is changed for the benchmark: the baseline measures the machine as
the user plays on it. If a setting changes (driver, Windows update, game
patch, power plan), record a new baseline.

## Procedure

1. Start the game, enter the scene, go to the spot.
2. **Warm-up:** stay in the scene for about 2 minutes before the first
   capture. Clocks, temperatures and game streaming settle during this time.
3. Run the script from the repository:

   ```powershell
   .\scripts\bench-baseline.ps1 -Label range-static `
       -PresentMon D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe `
       -Notes 'fps limit off'
   ```

4. Accept the UAC prompt, then click back into the game (20 s).
5. **Captures:** 5 captures of 60 s (`-Rounds 5`). A high beep marks the
   start of a capture, two short beeps its end. Keep still during captures;
   in the pause, do the gap action (tap crouch in The Range).
6. The script checks that every capture produced data (files
   `run-<round>-<condition>.csv`) and runs `bench frames`, which prints the
   table and writes `frames-summary.json`.

A **capture** here is one 60 s PresentMon recording. Repeating it 5 times in
the same scene shows how much the numbers vary on their own.

## Metrics

Computed per capture from PresentMon's `MsBetweenPresents` (time between the
game's presents, i.e. the game's own frame cadence):

| Metric | Definition |
|---|---|
| Average FPS | frames x 1000 / sum of frame times (not the mean of per-frame FPS) |
| 1% low | 1000 / mean of the slowest 1% of frame times |
| 0.1% low | 1000 / mean of the slowest 0.1% of frame times |
| p95, p99 frame time | nearest-rank percentiles (same definition as `telemetry`) |
| Max frame time | slowest frame |

The slowest N% is `max(1, floor(frames x N / 100))` frames. Rows without a
numeric frame time (`NA`) are skipped and counted.

Across captures: mean, sample standard deviation (n - 1), coefficient of
variation (sd / mean) and min..max.

## Noise floor and comparisons

The coefficient of variation of the baseline is the noise floor: a difference
smaller than it cannot be attributed to ziz.

When comparing with ziz running:

- Same scene, spot, settings and session; same build of ziz (release).
- **Interleave** conditions (without, with, without, with, ...) instead of all
  "without" first, so drift over time (temperature, background tasks) affects
  both sides equally.
- Compare means and look at whether the difference exceeds the noise of both
  sides. Report all numbers, including when there is no measurable difference.
- Do not compare captures from different sessions or spots: the first
  sessions showed a 2-3% average FPS difference between two spots.

## Capture A/B sessions

To measure what a capture method costs the game, pass several conditions:

```powershell
.\scripts\bench-baseline.ps1 -Label range-capture-ab `
    -Conditions none,wgc,dxgi -Rounds 5 `
    -PresentMon D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe
```

- `none` is the baseline (no capture); `wgc` and `dxgi` run
  `bench capture --backend <name> --monitor 0 --fps 60 --no-cursor` during
  the PresentMon capture, started 5 s before it (`-CaptureLead`) and stopped
  5 s after. The capture consumer releases every frame at once: this measures
  capture alone, not encoding.
- Each round runs every condition once; the order rotates every round
  (none-wgc-dxgi, wgc-dxgi-none, dxgi-none-wgc, ...), so no condition
  always gets the same position in the session.
- Each capture also writes `capture-<round>-<condition>.json`: delivered
  frame rate, counters (rate drops, pool drops, DXGI access losses), capture
  thread time per delivered frame and the CPU of the whole capture process
  (% of one core, which includes frames dropped by the rate limiter).
- At the end the script runs `bench ab <folder>`, which writes
  `ab-summary.json`. For each metric it reports the difference of means in
  percent of the baseline with a Welch 95% confidence interval, and says
  `Better` or `Worse` only when the interval excludes zero **and** the
  difference is larger than the baseline's coefficient of variation;
  otherwise `NoMeasurableDifference` or `WithinNoise`.

### Any program as a condition

`-Programs` adds conditions that run any program during the capture (for
example an app left open behind the game), as `name=path|arguments`;
`{seconds}` in the arguments becomes the capture length, leads included:

```powershell
.\scripts\bench-baseline.ps1 -Label range-app-ab -Conditions none,app -Rounds 5 `
    -Programs 'app=C:\path\app.exe|--background --seconds {seconds}' `
    -PresentMon D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe
```

The program is started without a console window (a console would take the
focus from the game) and stopped after the capture if it is still running.
It must open its window without activating it; a game that loses the focus
drops its frame rate (about 30 fps when in the background), which shows in
the results. Several entries are separated by commas or `;;`.

## Encoder comparisons

Two questions, measured separately: what an encoder costs the game, and how
good its picture is at a given bitrate.

### Impact on the game

The same A/B sessions accept two more conditions, `mf` and `amf`. Each runs
`bench encode --source monitor --encoder <name>`: WGC capture of the
monitor, NV12 conversion and H.264 encoding at `-EncodeKbps` (default
10000), on one thread (a stand-in for the real pipeline). Its
`capture-<round>-<condition>.json` adds encoder latency, busy drops and
the process CPU, and `encode-<round>-<condition>.h264` keeps the stream.

```powershell
.\scripts\bench-baseline.ps1 -Label range-encode-ab `
    -Conditions none,mf,amf -Rounds 5 `
    -PresentMon D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe
```

Other programs that use the GPU's video encoder change the result: with OBS
open and its replay buffer recording, encoders share the hardware with it.
`environment.json` records whether OBS was running (`obs_running`). Run the
isolated comparison with OBS closed, and the realistic one with OBS and its
replay buffer on, as separate sessions with labels that say which.

### Picture quality

`scripts/encode-quality.ps1` compares encoders on the same recorded footage:

```powershell
# 1. With the scene on screen: a lossless clip of the monitor (WGC capture).
.\scripts\encode-quality.ps1 -Record -Clip D:\Projects\live\lab\quality\range.mkv -RecordSeconds 20
# 2. Offline: references, encodes and metrics.
.\scripts\encode-quality.ps1 -Clip D:\Projects\live\lab\quality\range.mkv
```

- The clip is converted to raw NV12 references (BT.709, limited range, the
  converter's convention) at 1920x1080 and 1280x720.
- `bench encode --source raw` feeds each reference to each encoder at each
  bitrate, encoding every frame exactly once (it waits instead of dropping
  when the encoder is busy), so output and reference line up frame by frame.
  The raw source reads frames on the CPU: bench-only, never in the app.
- The output is decoded to raw NV12 without any range conversion and
  compared with the reference by ffmpeg (`psnr`, `ssim`, `libvmaf` with its
  default model). Results: `quality.csv` and `quality.json`.
- Compare at the bitrate each encoder actually produced (`achieved_mbps`),
  not only the target: on easy content an encoder may stay far below a CBR
  target (AMF's ultra-low-latency usage did, without filler data) while
  another pads up to it.
- Only the encoders' latency is timing-sensitive here; the bitstreams, and so
  the quality numbers, do not depend on system load.
## Pipeline stage costs

`bench pipeline` runs the app's host pipeline: the capture thread (WGC, or
DXGI if WGC cannot start) hands the newest frame to the encode thread, which
converts it to NV12 and drives the encoder (AMF on AMD GPUs, Media
Foundation elsewhere or if AMF cannot start), and the access units go to a
bounded queue. `--until convert` stops after the conversion. Its summary
reports each stage: capture cost, conversion GPU time, submit CPU time,
encoder latency, capture-to-output time (signed: WGC timestamps run a few
milliseconds ahead), the queue counters and the process CPU.

To see what each stage costs the game, compare them in one session:

```powershell
.\scripts\bench-baseline.ps1 -Label range-pipeline-ab `
    -Conditions none,wgc,convert,pipeline -Rounds 5 `
    -PresentMon D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe
```

- `wgc`: capture only (`bench capture`); `convert`: capture + NV12
  conversion; `pipeline`: the complete pipeline at `-EncodeKbps`.
- `pipeline` asks WGC for about one frame per period (`MinUpdateInterval`
  = 0.95 of it, the default); `rate` passes
  `-WgcIntervalFactor` explicitly (0.5 = up to twice the rate, the old
  default) and `rateprio` adds below-normal priority for the capture and
  encode threads (no measurable gain, so off by default). The rate
  limiter delivers the first frame of each fixed slot of one period, so a
  source a little faster than the target still delivers every slot.
- `bench ab` also compares the game's mean CPU busy, GPU busy and
  in-`Present` times per frame (PresentMon 2.x columns), which show where
  an added cost lands (a CPU-bound game shows it as CPU busy time).
- Record whether OBS was running (`environment.json`, `obs_running`): its
  replay buffer shares the GPU's video encoder and lowers the baseline.
- Before analyzing, check every run's frame rate over time (e.g. per 10 s):
  a run with the game in the background (capped at about 30 fps) or out of
  the scene is invalid and is replaced by an extra round, not trimmed.
## Decoding

`bench decode <file.h264>` splits an Annex B file into access units and
decodes them with Media Foundation's H.264 decoder using DXVA on the GPU:
pictures come out as NV12 slices of a D3D11 texture array and are released
right away, as a presenter would. It reports frames, picture sizes, the
submit-to-output time and, with `--stamps` (synthetic sources at their
original size), the frame stamp of every picture, read from the 1024x32
stamp region only (no whole-frame readback).

```powershell
.\target\release\bench.exe decode D:\Projects\live\lab\encode\<run>\<file>.h264 --stamps
```

- `--fps 60` (default) submits at the source rate; `--fps 0` as fast as the
  decoder takes access units (throughput).
- The submit-to-output time is the CPU hand-off: in low-latency mode the
  decoder returns each picture right after its input, and the GPU finishes
  decoding it in order before any later use. The end-to-end time from
  capture to display is measured in the loopback.
## Presenter checks

`bench present <file.h264>` decodes a file and shows it in the video
presenter's window at the source rate (`--repeat` loops it). The presenter
copies each decoded picture on the GPU, keeps only the newest one waiting
and shows it on the next refresh of the monitor; its window thread only
handles messages, so playback continues while the window is dragged or
resized. F11 or Alt+Enter toggle a borderless fullscreen window; O toggles
the statistics overlay.

```powershell
.\target\release\bench.exe present D:\Projects\live\lab\baseline\<session>\encode-1-pipeline.h264 --repeat
```

At the end it prints frames submitted, presented, replaced (a newer one
arrived before the screen took the waiting one), dropped and redrawn, and
the time from the decoder's hand-off to the `Present` call. Check by eye:
smooth playback, video keeps playing while the window moves or is resized,
the picture keeps its aspect ratio (black bars), fullscreen and back.

The presenter's GPU tests check what reaches the screen by capturing the
presenter's own window with WGC and reading the frame stamp from it.
## Loopback latency

`bench loopback` runs the whole path in one process: a source window shows
synthetic stamped frames, the host pipeline captures it (WGC), converts and
encodes it, and the access units go straight to the decoder and a viewer
window. The stamp carries the QPC time each frame was rendered, so latency
needs no clock sync:

- render -> decoded: the decoder hands a picture out (every Nth picture's
  stamp is read from its stamp region, `--decoded-stamp-every`);
- render -> on GPU: the same pictures once the decode has finished on the
  GPU (the stamp read waits for it);
- render -> displayed: the refresh (vblank) at which the viewer window
  showed the same sampled pictures, from the swap chain's DXGI frame
  statistics (`GetFrameStatistics` matched with `GetLastPresentCount`).
  It needs no probe and no extra tool, but in a composed (windowed) swap
  chain it reads late: against PresentMon, by one refresh in the loopback
  (3.0 ms at 320 Hz) and by one to two refreshes in `bench present`. Use it
  to watch trends; for the real number line it up with PresentMon (below);
- render -> screen: a WGC capture of the viewer window arrives, minus the
  stamp read from it. This includes both windows' composition and the probe
  capture itself, and it depends on how the probe's and the windows'
  updates line up with the compositor; treat it as an estimate only
  (`--no-probe` turns it off).

The viewer side also reports decoded -> displayed and present -> displayed
for every frame shown, and how many frames were matched to a refresh
(`matched`), placed from a later vblank (`estimated`) or not placed at all
(`unknown`: shown and replaced between two statistics reads, never shown,
or still pending at the end). "render -> source shown" is the same for the
source window; it is mostly `unknown` while the viewer window covers it.
`bench present` prints present -> displayed too.

```powershell
.\target\release\bench.exe loopback --width 1920 --height 1080 --kbps 10000 --encoder auto --seconds 30
```

To time the screen with PresentMon (the reference: kernel events of the
actual flip), start PresentMon on `bench.exe` with QPC timestamps, run the
loopback with `--display-csv`, then line the two up with
`scripts/display-latency.ps1`, which pairs each frame with the viewer's
PresentMon present by QPC time (`TimeInQPC + MsUntilDisplayed`). This is
when scan-out started: compositor included, the panel's response time not.

```powershell
$dir = 'D:\Projects\live\lab\loopback\<date>\presentmon'
Start-Process D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe -ArgumentList @('--process_name', 'bench.exe', '--qpc_time', '--output_file', "$dir\run-presentmon.csv", '--timed', '28', '--terminate_after_timed')
.\target\release\bench.exe loopback --seconds 30 --no-probe --display-csv "$dir\run-frames.csv"
.\scripts\display-latency.ps1 -Dir $dir
```

`--monitor N` captures a real monitor instead (no stamps, only per-stage
times and counters). The summary also lists every queue's counters, the
per-stage times of host and viewer, the process CPU and the bitrate.
## Viewer cost

What watching a stream costs, with the real viewer path only (hardware
decode + `VideoPresenter`, no encoder): `bench present` plays an Annex B
file at its frame rate, here on a second monitor without taking the focus.

```powershell
.\scripts\viewer-cost.ps1 -Files D:\Projects\live\lab\viewer\streams\1080p60.h264,D:\Projects\live\lab\viewer\streams\1080p30.h264 -At -1800,132
```

It reports CPU, wake-ups (context switches of the threads alive over the
window), memory, and GPU use **per engine type** from the raw engine
counters (3D, video decode — "Video Codec Engine" on AMD —, copy, other);
the game competes mostly for the 3D engine. The playback rate comes from
the file name (`1080p30` plays at 30 fps). The script keeps the displays on
while it measures (a display put to sleep by the power plan stops the
composition), without changing any setting.

Player options under test go through `-ExtraArgs` (e.g. `--gpu-priority=-7`,
`--skip-hidden`); `-Minimize` minimizes the player once it is up, to
measure what a hidden viewer costs.

In the game, the same viewer runs as a program condition:

```powershell
.\scripts\bench-baseline.ps1 -Label range-viewer -Conditions none,v1080p30 -Rounds 5 `
    -Programs 'v1080p30=D:\Projects\live\ziz\target\release\bench.exe|present "D:\Projects\live\lab\viewer\streams\1080p30.h264" --fps 30 --repeat --background --at=-1800,132 --seconds {seconds}' `
    -PresentMon D:\Tools\PresentMon\PresentMon-2.6.0-x64.exe
```

Measure both with the game uncapped (worst case: every bit of GPU time
counts) and capped as players use it; with a cap, the average frame rate
stays at the cap, so compare the lows, p99 and GPU busy time.

## Host and viewers in separate processes

The app itself (`ziz`) runs as host or viewer, each instance with its own
profile (data directory, logs and identity key under
`%LOCALAPPDATA%\ziz-dev\profiles\<name>\`); the viewer finds the host
through local signaling on 127.0.0.1 and receives the stream over WebRTC
(str0m).

Signaling is signed: each profile has an Ed25519 key, created on first use
and stored with DPAPI. The host only answers viewers whose key it was given
(`--trust KEY`, repeatable; `--trust-anyone` accepts any correctly signed
viewer, for measurements on one PC), and the viewer only accepts an answer
signed by `--host-key KEY`. `--print-key` prints a profile's key code.

```powershell
$hostKey = .\target\release\ziz.exe --profile test-host --print-key
.\target\release\ziz.exe --role host --profile test-host --synthetic --seconds 40 --trust-anyone
.\target\release\ziz.exe --role viewer --profile test-viewer --host-key $hostKey --at=-1800,132 --seconds 30 --summary-json D:\Projects\live\lab\c32\viewer.json
```

With `--synthetic` the host shares a stamped window, and the viewer reads
the stamp back from every 10th decoded frame: QPC is one clock for every
process on the PC, so render-to-decoded latency needs no clock sync. The
viewer summary also has the queue wait (network to decoder thread) and the
decode time. Display latency from DXGI frame statistics is unreliable while
the desktop composes (see "Loopback latency"); use PresentMon for it.
`--monitor N` shares a real monitor instead (keep the viewer window off it).

### Several viewers (fan-out)

Start more viewers against the same host, each with its own profile. One
encoder serves them all; the host logs per-viewer counters (estimate,
frames sent and skipped, times behind) every 5 s and at the end.

- Host cost: measure the host process (CPU time, GPU engines "3D" and
  "Video Codec" from the GPU performance counters) with 1, 3 and 5 viewers.
  Minimize the viewer windows for this: viewers drawing on the same desktop
  slow down the host's capture (same compositor), which is not host cost.
- Slow viewer: `--limit-kbps N` on one viewer puts an N kbps bottleneck
  (100 ms queue, then drops) in front of it. Expect the other viewers to
  keep every frame and the encoder bitrate to stay put; the slow viewer
  skips to keyframes.
- Late viewer: join while the stream runs; it starts at the next keyframe.

### Audio with the video

`--audio auto|synthetic|system|app --audio-pid N` on the host sends Opus
audio with the video, both stamped from one media-time origin. `auto`
follows the source: sharing a window (`--window "title"`) sends only that
window's app (its process tree, found from the window; Store apps through
the app behind `ApplicationFrameHost.exe`); sharing a monitor sends every
app playing except Discord (`Discord.exe`, `DiscordPTB.exe`,
`DiscordCanary.exe`: friends in the voice chat must not hear themselves),
any `ziz.exe` (no echo of streams this PC watches) and each
`--audio-exclude NAME`. Process loopback can leave out only one process
tree per capture, so that mode captures each playing app on its own
(include mode) and mixes them on one timeline, 40 ms behind real time; a
process whose tree holds an excluded one is split into its other playing
descendants, and an app that starts playing joins on Windows' new-session
notice (checked at least every second). `system` captures everything
except the host process, so a viewer playing on the same PC would be
captured again (echo).
The viewer plays it (60 ms held ahead of the speakers, drift corrected);
`--mute` and `--headless` decode without playing. Its summary has an
`audio` section: packets, concealed losses, gaps, underruns, playback
level, catch-ups (late audio dropped after a stall), output clock drift, and with a `--synthetic` host (whose video
stamps give the media-time origin) the time from the sound on the host to
heard here and the A/V offset against the video latency.

### Headless viewer and impaired networks

`--headless` runs the viewer without a window: it checks every frame it
receives (well-formed Annex B, SPS and PPS before each IDR, media time
moving forward) and writes the counts to its summary. `--headless --decode`
also decodes on the GPU, presenting nothing. Without decoding it needs no
GPU, which makes it the sink for measuring the host alone.

The viewer can impair what it receives (`transport::impair`; seeded, so a
run makes the same loss decisions for the same packets):

| Flag | Effect |
|---|---|
| `--loss P` | drop each datagram with probability P |
| `--burst P,N` | at each datagram, start a burst of N drops with probability P |
| `--delay-ms D --jitter-ms J` | delay D ± J ms, order kept |
| `--reorder P` | hold a datagram back so later ones overtake it |
| `--limit-kbps N --queue-ms Q` | N kbps bottleneck with a Q ms queue |
| `--outage AT,LEN` | drop everything from AT to AT+LEN seconds after joining |
| `--stall AT,LEN` | hold everything from AT to AT+LEN, then deliver it at once (a hiccup) |
| `--seed S` | seed of the loss and jitter decisions |

The summary adds what the impairment dropped and the longest gap between
frames handed to the decoder (the longest freeze). `cargo test -p transport
--test scenarios` runs the same cases (and peers vanishing, garbage
datagrams, an idle host) without the app or a GPU.

## Network addresses (ICE)

`bench net` shows what ICE would offer from this PC: the local addresses
by kind (physical, virtual; Radmin VPN addresses are skipped), the public
address two STUN servers see (with their round-trip times) and a NAT hint:
the same public port
from both servers means direct P2P usually works; different ports mean a
symmetric NAT. It sends only STUN requests. `--lan-only` skips STUN.

Host and viewer take `--network loopback|lan|internet` (default
`loopback`). `lan` binds one socket per adapter address; `internet` also
asks the STUN servers. Candidate priorities: physical adapters, then public
addresses, then virtual adapters (WSL, VPNs, IPv6 tunnels such as Teredo).
The viewer summary's `path` says which addresses the media
used. Listening on the LAN makes Windows Firewall ask for permission once
per program (allow private networks). `cargo test -p transport --test
network -- --ignored` connects host and viewer over the LAN addresses.

## NAT test for friends (`nat-probe`)

`nat-probe.exe` (`apps/nat-probe`, under 1 MB, network only) is what a
friend runs to see whether direct connections can work from their PC. It
asks for consent (text in Portuguese), then in about 25 s:

- one UDP socket asks four STUN servers on different addresses (Cloudflare,
  Google stun, stun1, stun2); the same public port from all of them means
  an endpoint-independent mapping (hole punching usually works), different
  ports mean a symmetric NAT;
- asks the router over UPnP, read only (`GetExternalIPAddress`, no port
  mappings), for its external address: in 100.64.0.0/10 means CGNAT, a
  private one means double NAT, a public one different from what STUN sees
  means another NAT after the router; no UPnP leaves it inconclusive;
- checks for native IPv6 (Teredo and 6to4 do not count) and asks two STUN
  servers over IPv6;
- asks one server again after `--wait` seconds (default 25) to see whether
  the mapping holds.

It prints a verdict (direct connection likely, hard, unlikely or
inconclusive), copies a short summary to the clipboard and writes
`nat-probe-resultado.json` next to the program. Neither contains a public
IP address unless `--show-ip` is given. `--yes` skips the consent and the
final pause, `--out` picks the JSON path, `--no-clipboard` leaves the
clipboard alone. `cargo test -p nat-probe -- --ignored` runs it against the
real network.

## Manual invites (no server)

A direct link between two PCs without a signaling server, for tests with
friends and as a fallback: one viewer watching one host, with no room
features. Here the host makes the offer: `--invite` puts a signed invite
text (`ziz1:...`, about 2,100 characters) on the clipboard. The friend
copies it and runs `--join`, and their signed answer goes to their
clipboard. The host copies that answer and presses Enter. Both default to
`--network internet`. Each invite takes one answer and lasts ten minutes.

```powershell
.\target\release\ziz.exe --role host --profile me --synthetic --invite
.\target\release\ziz.exe --role viewer --profile friend --join
```

A key that is not in the profile's `contacts.json` is shown with a
question (compare its start with the other person if you can). A yes saves
it, so the next time nobody is asked. `--trust KEY` (host) and
`--host-key KEY` (viewer) count as a yes. `--invite-file` and
`--answer-file` use files instead of the clipboard, for scripts. For
mutual sharing, use one invite per direction.

The text is the signed envelope compressed with the Windows Compression
API (XPRESS Huffman) in base64url. Offers carry one H.264 format (High,
packetization mode 1), not str0m's seven, which keeps them short.

## Audio in one process

`bench audio` runs the audio path without a network: capture, Opus
(48 kHz stereo, 20 ms frames), decode, playback on the default output.

```powershell
.\target\release\bench.exe audio --source synthetic --seconds 600 --summary-json D:\Projects\live\lab\c35\synthetic.json
.\target\release\bench.exe audio --source system --seconds 60
.\target\release\bench.exe audio --source system-except --seconds 60
.\target\release\bench.exe audio --source app --pid 1234 --seconds 60
```

- Sources: `system` (everything playing except this process, so playback is
  never captured again), `system-except` (also without Discord and ziz,
  each app captured on its own and mixed: what sharing a monitor uses),
  `app --pid` (that process tree only), `synthetic` (a quiet tone with a
  loud click once a second, on the QPC clock).
- `cargo test -p audio --test app_audio -- --ignored --test-threads=1`
  checks with real sound which apps a capture takes: copies of the
  `tone-player` test helper stand in for Discord (1 kHz) and other apps
  (440 Hz) and play quietly for a few seconds.
- Reported: Opus bitrate, encode/decode time per frame, capture to decoded,
  the playback level (queue + device buffer) the drift controller holds,
  its resampling ratio, underruns, the output device clock against QPC
  (ppm), process CPU and private memory. `--no-play` measures the codec alone.
- Building needs libopus from vcpkg (`vcpkg install opus:x64-windows-static-md`);
  `.cargo/config.toml` points `VCPKG_ROOT` at it unless the environment does.

## First baseline (2026-11-18)

VALORANT, The Range, fixed spot, 1920x1080, FPS limit off, RX 7600, driver
32.0.31021.5001, Windows 25H2 build 26200.9974, power plan Balanced.
5 x 60 s:

| | Average FPS | 1% low | 0.1% low | p95 | p99 |
|---|---|---|---|---|---|
| Mean | 547.9 | 310.6 | 227.9 | 2.455 ms | 2.864 ms |
| Coefficient of variation | 1.2% | 2.1% | 8.8% | 1.6% | 1.2% |

Average FPS, 1% low and p95/p99 have a noise floor of about 1-2%. The 0.1%
low rests on about 33 frames per capture and varies too much (about 9%) for
small differences; use longer captures or more of them if it matters.
