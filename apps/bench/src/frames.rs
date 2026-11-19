//! `bench frames`: frame-pacing metrics from PresentMon CSV captures, per
//! run and across runs. Definitions are documented in
//! docs/benchmark-protocol.md and must stay stable so baselines remain
//! comparable.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use telemetry::{QpcDuration, QpcFrequency, Summary};

/// Frame-time column names, in order of preference. PresentMon 1.x and the
/// 2.x default output use `MsBetweenPresents`; some 2.x builds use `FrameTime`.
const FRAME_TIME_COLUMNS: [&str; 2] = ["MsBetweenPresents", "FrameTime"];
const APPLICATION_COLUMN: &str = "Application";

/// Ticks per second used to reuse `telemetry::Summary` (nearest-rank
/// percentiles) on millisecond frame times; 0.1 us resolution.
const SUMMARY_FREQUENCY: u64 = 10_000_000;

#[derive(Debug, PartialEq, Eq)]
pub enum FramesError {
    Empty,
    MissingColumn(&'static str),
    MultipleApplications(Vec<String>),
    NoFrames { application: Option<String> },
}

impl fmt::Display for FramesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FramesError::Empty => write!(f, "the CSV file is empty"),
            FramesError::MissingColumn(name) => write!(f, "missing column {name}"),
            FramesError::MultipleApplications(names) => write!(
                f,
                "the capture contains several applications ({}); pick one with --process",
                names.join(", ")
            ),
            FramesError::NoFrames { application: None } => write!(f, "no frames in the capture"),
            FramesError::NoFrames {
                application: Some(name),
            } => write!(f, "no frames for application {name}"),
        }
    }
}

impl Error for FramesError {}

/// PresentMon 2.x columns with where each frame's time went.
const CPU_BUSY_COLUMN: &str = "MsCPUBusy";
const GPU_BUSY_COLUMN: &str = "MsGPUBusy";
const IN_PRESENT_COLUMN: &str = "MsInPresentAPI";

/// Mean per-frame busy times of the game, when the capture has the columns:
/// CPU busy (the app's CPU work for the frame), GPU busy and time inside
/// `Present`. They show which side an added cost lands on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct GameBusy {
    pub cpu_busy_ms: Option<f64>,
    pub gpu_busy_ms: Option<f64>,
    pub in_present_ms: Option<f64>,
}

/// Frame times of one application in one capture.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameCapture {
    pub application: String,
    pub frame_times_ms: Vec<f64>,
    /// Rows whose frame time was not a number (e.g. `NA` on the first frame).
    pub skipped_rows: usize,
    pub busy: GameBusy,
}

/// Running mean of one optional column.
#[derive(Default)]
struct ColumnMean {
    sum: f64,
    count: usize,
}

impl ColumnMean {
    fn add(&mut self, field: Option<&&str>) {
        if let Some(Ok(value)) = field.map(|text| text.trim().parse::<f64>())
            && value.is_finite()
        {
            self.sum += value;
            self.count += 1;
        }
    }

    fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }
}

/// Parses a PresentMon CSV. Columns are found by name, so 1.x and 2.x
/// layouts both work. With `process`, only rows of that application are kept.
pub fn parse_presentmon_csv(
    text: &str,
    process: Option<&str>,
) -> Result<FrameCapture, FramesError> {
    // PresentMon writes a UTF-8 byte order mark before the header.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header: Vec<&str> = lines
        .next()
        .ok_or(FramesError::Empty)?
        .split(',')
        .map(str::trim)
        .collect();
    let column = |name: &'static str| header.iter().position(|&column| column == name);
    let application_index =
        column(APPLICATION_COLUMN).ok_or(FramesError::MissingColumn(APPLICATION_COLUMN))?;
    let frame_time_index = FRAME_TIME_COLUMNS
        .iter()
        .find_map(|&name| column(name))
        .ok_or(FramesError::MissingColumn(FRAME_TIME_COLUMNS[0]))?;

    let busy_columns = [CPU_BUSY_COLUMN, GPU_BUSY_COLUMN, IN_PRESENT_COLUMN].map(column);
    let mut busy_means: [ColumnMean; 3] = Default::default();

    let mut applications: Vec<String> = Vec::new();
    let mut frame_times_ms = Vec::new();
    let mut skipped_rows = 0;
    for line in lines {
        let fields: Vec<&str> = line.split(',').collect();
        let (Some(application), Some(frame_time)) =
            (fields.get(application_index), fields.get(frame_time_index))
        else {
            skipped_rows += 1;
            continue;
        };
        let application = application.trim();
        if process.is_some_and(|wanted| !wanted.eq_ignore_ascii_case(application)) {
            continue;
        }
        // Executable names are case-insensitive on Windows.
        if !applications
            .iter()
            .any(|known| known.eq_ignore_ascii_case(application))
        {
            applications.push(application.to_owned());
        }
        match frame_time.trim().parse::<f64>() {
            Ok(value) if value.is_finite() && value > 0.0 => {
                frame_times_ms.push(value);
                for (index, mean) in busy_columns.iter().zip(busy_means.iter_mut()) {
                    if let Some(index) = index {
                        mean.add(fields.get(*index));
                    }
                }
            }
            _ => skipped_rows += 1,
        }
    }

    if applications.len() > 1 {
        return Err(FramesError::MultipleApplications(applications));
    }
    if frame_times_ms.is_empty() {
        return Err(FramesError::NoFrames {
            application: process.map(str::to_owned),
        });
    }
    Ok(FrameCapture {
        application: applications.remove(0),
        frame_times_ms,
        skipped_rows,
        busy: GameBusy {
            cpu_busy_ms: busy_means[0].mean(),
            gpu_busy_ms: busy_means[1].mean(),
            in_present_ms: busy_means[2].mean(),
        },
    })
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FrameTimeStats {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// Metrics of one capture.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunMetrics {
    pub file: String,
    pub application: String,
    pub frames: usize,
    pub skipped_rows: usize,
    pub duration_s: f64,
    /// Frames divided by total time (not the mean of per-frame FPS).
    pub avg_fps: f64,
    /// 1000 / mean of the slowest 1% of frame times.
    pub low_1pct_fps: f64,
    /// 1000 / mean of the slowest 0.1% of frame times.
    pub low_01pct_fps: f64,
    pub frame_time: FrameTimeStats,
    pub game_busy: GameBusy,
}

impl RunMetrics {
    pub fn from_capture(file: String, capture: &FrameCapture) -> Self {
        let frame_times = &capture.frame_times_ms;
        let total_ms: f64 = frame_times.iter().sum();
        let mut slowest_first = frame_times.clone();
        slowest_first.sort_unstable_by(|a, b| b.total_cmp(a));

        let frequency = QpcFrequency::new(SUMMARY_FREQUENCY).expect("non-zero frequency");
        let durations: Vec<QpcDuration> = frame_times
            .iter()
            .map(|&ms| QpcDuration::from_micros(ms * 1_000.0, frequency))
            .collect();
        let summary =
            Summary::from_durations(&durations, frequency).expect("captures are never empty");

        Self {
            file,
            application: capture.application.clone(),
            frames: frame_times.len(),
            skipped_rows: capture.skipped_rows,
            duration_s: total_ms / 1_000.0,
            avg_fps: frame_times.len() as f64 * 1_000.0 / total_ms,
            low_1pct_fps: 1_000.0 / mean_of_slowest(&slowest_first, 0.01),
            low_01pct_fps: 1_000.0 / mean_of_slowest(&slowest_first, 0.001),
            frame_time: FrameTimeStats {
                p50_ms: summary.p50_us / 1_000.0,
                p95_ms: summary.p95_us / 1_000.0,
                p99_ms: summary.p99_us / 1_000.0,
                max_ms: summary.max_us / 1_000.0,
            },
            game_busy: capture.busy,
        }
    }
}

/// Mean of the slowest `fraction` of frames: `max(1, floor(n * fraction))`
/// frames from a slowest-first list.
fn mean_of_slowest(slowest_first: &[f64], fraction: f64) -> f64 {
    let count = ((slowest_first.len() as f64 * fraction).floor() as usize).max(1);
    slowest_first[..count].iter().sum::<f64>() / count as f64
}

/// Spread of one metric across runs.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Spread {
    pub mean: f64,
    /// Sample standard deviation (n - 1); zero for a single run.
    pub sd: f64,
    /// Standard deviation as a percentage of the mean: the noise floor.
    pub cv_percent: f64,
    pub min: f64,
    pub max: f64,
}

impl Spread {
    pub(crate) fn of(values: &[f64]) -> Self {
        let count = values.len() as f64;
        let mean = values.iter().sum::<f64>() / count;
        let sd = if values.len() > 1 {
            (values
                .iter()
                .map(|value| (value - mean).powi(2))
                .sum::<f64>()
                / (count - 1.0))
                .sqrt()
        } else {
            0.0
        };
        Self {
            mean,
            sd,
            cv_percent: 100.0 * sd / mean,
            min: values.iter().copied().fold(f64::INFINITY, f64::min),
            max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AcrossRuns {
    pub runs: usize,
    pub avg_fps: Spread,
    pub low_1pct_fps: Spread,
    pub low_01pct_fps: Spread,
    pub p95_ms: Spread,
    pub p99_ms: Spread,
}

impl AcrossRuns {
    pub fn of(runs: &[RunMetrics]) -> Self {
        let spread = |metric: fn(&RunMetrics) -> f64| {
            Spread::of(&runs.iter().map(metric).collect::<Vec<_>>())
        };
        Self {
            runs: runs.len(),
            avg_fps: spread(|run| run.avg_fps),
            low_1pct_fps: spread(|run| run.low_1pct_fps),
            low_01pct_fps: spread(|run| run.low_01pct_fps),
            p95_ms: spread(|run| run.frame_time.p95_ms),
            p99_ms: spread(|run| run.frame_time.p99_ms),
        }
    }
}

#[derive(Debug, Serialize)]
struct FramesReport {
    schema_version: u32,
    app_version: &'static str,
    definitions: &'static str,
    runs: Vec<RunMetrics>,
    across_runs: AcrossRuns,
}

const DEFINITIONS: &str = "avg_fps = frames * 1000 / sum(frame times); low_Npct_fps = 1000 / mean of the slowest max(1, floor(frames * N/100)) frame times; percentiles are nearest-rank; sd is the sample standard deviation (n - 1)";

/// Analyzes `files`, prints a table and writes a JSON report.
pub fn run(
    files: &[PathBuf],
    process: Option<&str>,
    json: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let mut runs = Vec::with_capacity(files.len());
    for file in files {
        let text =
            fs::read_to_string(file).map_err(|error| format!("{}: {error}", file.display()))?;
        let capture = parse_presentmon_csv(&text, process)
            .map_err(|error| format!("{}: {error}", file.display()))?;
        runs.push(RunMetrics::from_capture(display_name(file), &capture));
    }
    let across_runs = AcrossRuns::of(&runs);

    println!(
        "{:<12} {:>7} {:>8} {:>9} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "run", "frames", "seconds", "avg fps", "1% low", "0.1% low", "p95 ms", "p99 ms", "max ms"
    );
    for run in &runs {
        println!(
            "{:<12} {:>7} {:>8.1} {:>9.1} {:>8.1} {:>8.1} {:>8.3} {:>8.3} {:>8.3}",
            run.file,
            run.frames,
            run.duration_s,
            run.avg_fps,
            run.low_1pct_fps,
            run.low_01pct_fps,
            run.frame_time.p95_ms,
            run.frame_time.p99_ms,
            run.frame_time.max_ms
        );
    }
    println!();
    println!(
        "across {} run(s): mean +- sd (cv), min..max",
        across_runs.runs
    );
    for (name, spread, decimals) in [
        ("avg fps", &across_runs.avg_fps, 1),
        ("1% low", &across_runs.low_1pct_fps, 1),
        ("0.1% low", &across_runs.low_01pct_fps, 1),
        ("p95 ms", &across_runs.p95_ms, 3),
        ("p99 ms", &across_runs.p99_ms, 3),
    ] {
        // One extra decimal on sd/cv so small spreads do not print as zero.
        println!(
            "  {name:<9} {:.*} +- {:.*} ({:.2}%), {:.*}..{:.*}",
            decimals,
            spread.mean,
            decimals + 1,
            spread.sd,
            spread.cv_percent,
            decimals,
            spread.min,
            decimals,
            spread.max
        );
    }

    let path = json.unwrap_or_else(|| {
        files[0]
            .parent()
            .unwrap_or(Path::new("."))
            .join("frames-summary.json")
    });
    let report = FramesReport {
        schema_version: 1,
        app_version: env!("CARGO_PKG_VERSION"),
        definitions: DEFINITIONS,
        runs,
        across_runs,
    };
    fs::write(&path, serde_json::to_string_pretty(&report)?)?;
    println!();
    println!("JSON: {}", path.display());
    Ok(())
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const V2_HEADER: &str = "Application,ProcessID,SwapChainAddress,PresentRuntime,SyncInterval,PresentFlags,AllowsTearing,PresentMode,TimeInQPC,MsBetweenSimulationStart,MsBetweenPresents,MsBetweenDisplayChange,CPUStartQPC";

    fn v2_csv(frame_times: &[&str]) -> String {
        let mut csv = format!("{V2_HEADER}\n");
        for (index, frame_time) in frame_times.iter().enumerate() {
            csv.push_str(&format!(
                "game.exe,42,0x1,DXGI,0,512,1,Hardware: Independent Flip,{index},NA,{frame_time},NA,{index}\n"
            ));
        }
        csv
    }

    fn capture(frame_times: Vec<f64>) -> FrameCapture {
        FrameCapture {
            application: "game.exe".to_owned(),
            frame_times_ms: frame_times,
            skipped_rows: 0,
            busy: GameBusy::default(),
        }
    }

    #[test]
    fn parses_v2_layout_and_skips_na() {
        let parsed = parse_presentmon_csv(&v2_csv(&["NA", "2.0", "2.5"]), None).unwrap();
        assert_eq!(parsed.application, "game.exe");
        assert_eq!(parsed.frame_times_ms, [2.0, 2.5]);
        assert_eq!(parsed.skipped_rows, 1);
    }

    #[test]
    fn ignores_utf8_byte_order_mark() {
        let csv = format!("\u{feff}{}", v2_csv(&["3.0"]));
        assert_eq!(
            parse_presentmon_csv(&csv, None).unwrap().frame_times_ms,
            [3.0]
        );
    }

    #[test]
    fn averages_busy_columns_when_present() {
        let csv = "Application,MsBetweenPresents,MsInPresentAPI,MsCPUBusy,MsGPUBusy\n\
                   g.exe,NA,0.1,1.0,0.5\n\
                   g.exe,2.0,0.1,1.0,0.5\n\
                   g.exe,2.0,0.3,2.0,NA\n";
        let capture = parse_presentmon_csv(csv, None).unwrap();
        // The first row has no frame time and is skipped entirely.
        assert_eq!(capture.frame_times_ms, [2.0, 2.0]);
        assert_eq!(capture.busy.in_present_ms, Some(0.2));
        assert_eq!(capture.busy.cpu_busy_ms, Some(1.5));
        assert_eq!(capture.busy.gpu_busy_ms, Some(0.5));
    }

    #[test]
    fn busy_times_are_absent_without_the_columns() {
        let capture =
            parse_presentmon_csv("Application,MsBetweenPresents\ng.exe,2.0\n", None).unwrap();
        assert_eq!(capture.busy, GameBusy::default());
    }
    #[test]
    fn parses_frame_time_alias() {
        let csv = "Application,ProcessID,FrameTime\ngame.exe,1,4.0\ngame.exe,1,6.0\n";
        let parsed = parse_presentmon_csv(csv, None).unwrap();
        assert_eq!(parsed.frame_times_ms, [4.0, 6.0]);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(parse_presentmon_csv("", None), Err(FramesError::Empty));
        assert_eq!(
            parse_presentmon_csv("ProcessID,MsBetweenPresents\n1,2.0\n", None),
            Err(FramesError::MissingColumn("Application"))
        );
        assert_eq!(
            parse_presentmon_csv("Application,ProcessID\ngame.exe,1\n", None),
            Err(FramesError::MissingColumn("MsBetweenPresents"))
        );
        assert_eq!(
            parse_presentmon_csv(&v2_csv(&["NA"]), None),
            Err(FramesError::NoFrames { application: None })
        );
    }

    #[test]
    fn several_applications_need_a_process_filter() {
        let csv = "Application,MsBetweenPresents\na.exe,1.0\nb.exe,2.0\nA.EXE,3.0\n";
        assert_eq!(
            parse_presentmon_csv(csv, None),
            Err(FramesError::MultipleApplications(vec![
                "a.exe".to_owned(),
                "b.exe".to_owned()
            ]))
        );
        let only_a = parse_presentmon_csv(csv, Some("a.exe")).unwrap();
        assert_eq!(only_a.frame_times_ms, [1.0, 3.0]);
        assert_eq!(
            parse_presentmon_csv(csv, Some("c.exe")),
            Err(FramesError::NoFrames {
                application: Some("c.exe".to_owned())
            })
        );
    }

    #[test]
    fn metrics_on_known_frames() {
        // 990 frames at 2 ms and 10 spikes at 10 ms.
        let mut frames = vec![2.0; 990];
        frames.extend([10.0; 10]);
        let metrics = RunMetrics::from_capture("run-1.csv".to_owned(), &capture(frames));

        assert_eq!(metrics.frames, 1_000);
        assert!((metrics.duration_s - 2.08).abs() < 1e-9);
        assert!((metrics.avg_fps - 1_000.0 / 2.08).abs() < 1e-9);
        // Slowest 1% = the 10 spikes; slowest 0.1% = 1 spike.
        assert!((metrics.low_1pct_fps - 100.0).abs() < 1e-9);
        assert!((metrics.low_01pct_fps - 100.0).abs() < 1e-9);
        assert!((metrics.frame_time.p50_ms - 2.0).abs() < 1e-6);
        assert!((metrics.frame_time.p99_ms - 2.0).abs() < 1e-6);
        assert!((metrics.frame_time.max_ms - 10.0).abs() < 1e-6);
    }

    #[test]
    fn slowest_fraction_rounds_down_with_minimum_one() {
        let slowest_first = [9.0, 7.0, 5.0, 3.0, 1.0];
        assert_eq!(mean_of_slowest(&slowest_first, 0.01), 9.0);
        assert_eq!(mean_of_slowest(&slowest_first, 0.4), 8.0);
    }

    #[test]
    fn spread_uses_sample_standard_deviation() {
        let spread = Spread::of(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]);
        assert!((spread.mean - 5.0).abs() < 1e-12);
        assert!((spread.sd - (32.0_f64 / 7.0).sqrt()).abs() < 1e-12);
        assert!((spread.cv_percent - 100.0 * spread.sd / 5.0).abs() < 1e-12);
        assert_eq!((spread.min, spread.max), (2.0, 9.0));
        assert_eq!(Spread::of(&[3.0]).sd, 0.0);
    }

    #[test]
    fn across_runs_collects_each_metric() {
        let fast = RunMetrics::from_capture("a".to_owned(), &capture(vec![2.0; 100]));
        let slow = RunMetrics::from_capture("b".to_owned(), &capture(vec![4.0; 100]));
        let across = AcrossRuns::of(&[fast, slow]);
        assert_eq!(across.runs, 2);
        assert!((across.avg_fps.mean - 375.0).abs() < 1e-9);
        assert_eq!((across.avg_fps.min, across.avg_fps.max), (250.0, 500.0));
        assert!((across.p99_ms.mean - 3.0).abs() < 1e-6);
    }
}
