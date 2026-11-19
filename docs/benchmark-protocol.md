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
