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
