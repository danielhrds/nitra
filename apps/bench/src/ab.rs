//! `bench ab <dir>`: compares frame-pacing metrics between conditions (e.g.
//! no capture, WGC, DXGI) recorded by `scripts/bench-baseline.ps1`, which
//! names captures `run-<round>-<condition>.csv` and capture summaries
//! `capture-<round>-<condition>.json`.
//!
//! Each condition is compared with the baseline condition by the difference
//! of means with a Welch 95% confidence interval. A difference is reported
//! only when the interval excludes zero *and* it exceeds the baseline's own
//! run-to-run variation (its coefficient of variation): see
//! docs/benchmark-protocol.md.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::frames::{AcrossRuns, RunMetrics, parse_presentmon_csv};

/// `run-<round>-<condition>.csv` -> (round, condition).
fn parse_run_name(name: &str) -> Option<(u32, String)> {
    let stem = name.strip_prefix("run-")?.strip_suffix(".csv")?;
    let (round, condition) = stem.split_once('-')?;
    let valid = !condition.is_empty()
        && condition
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid {
        return None;
    }
    Some((round.parse().ok()?, condition.to_owned()))
}

/// Two-sided 95% critical value of Student's t, rounding `df` down
/// (conservative).
fn t_critical_95(df: f64) -> f64 {
    const TABLE: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
        2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
        2.052, 2.048, 2.045, 2.042,
    ];
    let df = df.floor().max(1.0);
    match df as usize {
        index @ 1..=30 => TABLE[index - 1],
        31..=39 => 2.042,
        40..=59 => 2.021,
        60..=119 => 2.000,
        _ => 1.980,
    }
}

/// Difference of means (`other - base`) and its Welch 95% interval.
/// `None` when either side has fewer than two runs.
fn welch_interval(base: &[f64], other: &[f64]) -> Option<(f64, f64, f64)> {
    if base.len() < 2 || other.len() < 2 {
        return None;
    }
    let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
    let variance = |values: &[f64], mean: f64| {
        values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / (values.len() as f64 - 1.0)
    };
    let (base_mean, other_mean) = (mean(base), mean(other));
    let base_term = variance(base, base_mean) / base.len() as f64;
    let other_term = variance(other, other_mean) / other.len() as f64;
    let difference = other_mean - base_mean;
    let standard_error = (base_term + other_term).sqrt();
    if standard_error == 0.0 {
        return Some((difference, difference, difference));
    }
    let df = (base_term + other_term).powi(2)
        / (base_term.powi(2) / (base.len() as f64 - 1.0)
            + other_term.powi(2) / (other.len() as f64 - 1.0));
    let margin = t_critical_95(df) * standard_error;
    Some((difference, difference - margin, difference + margin))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    NoMeasurableDifference,
    WithinNoise,
    Better,
    Worse,
    InsufficientData,
}

#[derive(Debug, Serialize)]
struct Comparison {
    metric: &'static str,
    baseline_mean: f64,
    condition_mean: f64,
    delta_percent: f64,
    ci95_low_percent: Option<f64>,
    ci95_high_percent: Option<f64>,
    baseline_noise_percent: f64,
    verdict: Verdict,
}

fn compare(
    metric: &'static str,
    higher_is_better: bool,
    base: &[f64],
    other: &[f64],
    baseline_noise_percent: f64,
) -> Comparison {
    let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
    let (base_mean, other_mean) = (mean(base), mean(other));
    let percent = |value: f64| 100.0 * value / base_mean;
    let delta_percent = percent(other_mean - base_mean);
    let interval = welch_interval(base, other);
    let verdict = match interval {
        None => Verdict::InsufficientData,
        Some((_, low, high)) if low <= 0.0 && high >= 0.0 => Verdict::NoMeasurableDifference,
        Some(_) if delta_percent.abs() < baseline_noise_percent => Verdict::WithinNoise,
        Some(_) if (delta_percent > 0.0) == higher_is_better => Verdict::Better,
        Some(_) => Verdict::Worse,
    };
    Comparison {
        metric,
        baseline_mean: base_mean,
        condition_mean: other_mean,
        delta_percent,
        ci95_low_percent: interval.map(|(_, low, _)| percent(low)),
        ci95_high_percent: interval.map(|(_, _, high)| percent(high)),
        baseline_noise_percent,
        verdict,
    }
}

/// Means of the capture-side summaries written by `bench capture`.
#[derive(Debug, Default, Serialize)]
struct CaptureSide {
    summaries: usize,
    mean_taken_fps: Option<f64>,
    mean_thread_cpu_p50_us: Option<f64>,
    /// Whole capture process, % of one core (includes rate-dropped frames).
    mean_process_cpu_percent: Option<f64>,
    total_pool_dropped: u64,
    total_access_lost: u64,
}

#[derive(Debug, Serialize)]
struct ConditionReport {
    condition: String,
    runs: Vec<RunMetrics>,
    across_runs: AcrossRuns,
    capture: CaptureSide,
    comparisons: Vec<Comparison>,
}

#[derive(Debug, Serialize)]
struct AbReport {
    schema_version: u32,
    baseline: String,
    method: &'static str,
    conditions: Vec<ConditionReport>,
}

const METHOD: &str = "delta = condition mean - baseline mean (percent of baseline); 95% CI by Welch's t; verdict 'better'/'worse' only if the CI excludes 0 and |delta| exceeds the baseline coefficient of variation";

fn capture_side(dir: &Path, rounds: &[u32], condition: &str) -> CaptureSide {
    let mut side = CaptureSide::default();
    let (mut fps_sum, mut cpu_sum, mut cpu_count) = (0.0, 0.0, 0);
    let (mut process_cpu_sum, mut process_cpu_count) = (0.0, 0);
    for round in rounds {
        let path = dir.join(format!("capture-{round}-{condition}.json"));
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        side.summaries += 1;
        fps_sum += json["taken_fps"].as_f64().unwrap_or(0.0);
        if let Some(cpu) = json["thread_cpu_us"]["p50"].as_f64() {
            cpu_sum += cpu;
            cpu_count += 1;
        }
        if let Some(cpu) = json["process_cpu_percent"].as_f64() {
            process_cpu_sum += cpu;
            process_cpu_count += 1;
        }
        side.total_pool_dropped += json["counters"]["pool_dropped"].as_u64().unwrap_or(0);
        side.total_access_lost += json["counters"]["access_lost"].as_u64().unwrap_or(0);
    }
    if side.summaries > 0 {
        side.mean_taken_fps = Some(fps_sum / side.summaries as f64);
    }
    if cpu_count > 0 {
        side.mean_thread_cpu_p50_us = Some(cpu_sum / f64::from(cpu_count));
    }
    if process_cpu_count > 0 {
        side.mean_process_cpu_percent = Some(process_cpu_sum / f64::from(process_cpu_count));
    }
    side
}

pub fn run(
    dir: &Path,
    baseline: &str,
    process: Option<&str>,
    json: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    // condition -> [(round, metrics)]
    let mut groups: BTreeMap<String, Vec<(u32, RunMetrics)>> = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((round, condition)) = parse_run_name(name) else {
            continue;
        };
        let text = fs::read_to_string(&path)?;
        let capture =
            parse_presentmon_csv(&text, process).map_err(|error| format!("{name}: {error}"))?;
        groups
            .entry(condition)
            .or_default()
            .push((round, RunMetrics::from_capture(name.to_owned(), &capture)));
    }
    if !groups.contains_key(baseline) {
        return Err(format!(
            "no run-<round>-{baseline}.csv in {}; conditions found: {:?}",
            dir.display(),
            groups.keys().collect::<Vec<_>>()
        )
        .into());
    }

    let values = |runs: &[(u32, RunMetrics)], metric: fn(&RunMetrics) -> f64| -> Vec<f64> {
        runs.iter().map(|(_, run)| metric(run)).collect()
    };
    let baseline_runs = &groups[baseline];
    let baseline_across = AcrossRuns::of(
        &baseline_runs
            .iter()
            .map(|(_, run)| run.clone())
            .collect::<Vec<_>>(),
    );

    // Baseline first, then the others in name order.
    let mut names: Vec<&String> = groups
        .keys()
        .filter(|name| name.as_str() != baseline)
        .collect();
    names.insert(
        0,
        groups
            .keys()
            .find(|name| name.as_str() == baseline)
            .expect("checked"),
    );

    let mut conditions = Vec::new();
    for name in names {
        let mut runs = groups[name].clone();
        runs.sort_by_key(|(round, _)| *round);
        let rounds: Vec<u32> = runs.iter().map(|(round, _)| *round).collect();
        let metrics: Vec<RunMetrics> = runs.iter().map(|(_, run)| run.clone()).collect();
        let across_runs = AcrossRuns::of(&metrics);
        let comparisons = if name == baseline {
            Vec::new()
        } else {
            vec![
                compare(
                    "avg_fps",
                    true,
                    &values(baseline_runs, |r| r.avg_fps),
                    &values(&runs, |r| r.avg_fps),
                    baseline_across.avg_fps.cv_percent,
                ),
                compare(
                    "low_1pct_fps",
                    true,
                    &values(baseline_runs, |r| r.low_1pct_fps),
                    &values(&runs, |r| r.low_1pct_fps),
                    baseline_across.low_1pct_fps.cv_percent,
                ),
                compare(
                    "low_01pct_fps",
                    true,
                    &values(baseline_runs, |r| r.low_01pct_fps),
                    &values(&runs, |r| r.low_01pct_fps),
                    baseline_across.low_01pct_fps.cv_percent,
                ),
                compare(
                    "p95_ms",
                    false,
                    &values(baseline_runs, |r| r.frame_time.p95_ms),
                    &values(&runs, |r| r.frame_time.p95_ms),
                    baseline_across.p95_ms.cv_percent,
                ),
                compare(
                    "p99_ms",
                    false,
                    &values(baseline_runs, |r| r.frame_time.p99_ms),
                    &values(&runs, |r| r.frame_time.p99_ms),
                    baseline_across.p99_ms.cv_percent,
                ),
            ]
        };
        conditions.push(ConditionReport {
            condition: name.clone(),
            capture: capture_side(dir, &rounds, name),
            runs: metrics,
            across_runs,
            comparisons,
        });
    }

    print_report(&conditions);
    let path = json.unwrap_or_else(|| dir.join("ab-summary.json"));
    let report = AbReport {
        schema_version: 1,
        baseline: baseline.to_owned(),
        method: METHOD,
        conditions,
    };
    fs::write(&path, serde_json::to_string_pretty(&report)?)?;
    println!();
    println!("JSON: {}", path.display());
    Ok(())
}

fn print_report(conditions: &[ConditionReport]) {
    println!(
        "{:<10} {:>4} {:>16} {:>16} {:>16} {:>14} {:>14} {:>8} {:>10} {:>9}",
        "condition",
        "runs",
        "avg fps",
        "1% low",
        "0.1% low",
        "p95 ms",
        "p99 ms",
        "cap fps",
        "frame cpu",
        "proc cpu"
    );
    for condition in conditions {
        let across = &condition.across_runs;
        let spread =
            |mean: f64, sd: f64, decimals: usize| format!("{mean:.decimals$} +- {sd:.decimals$}");
        println!(
            "{:<10} {:>4} {:>16} {:>16} {:>16} {:>14} {:>14} {:>8} {:>10} {:>9}",
            condition.condition,
            across.runs,
            spread(across.avg_fps.mean, across.avg_fps.sd, 1),
            spread(across.low_1pct_fps.mean, across.low_1pct_fps.sd, 1),
            spread(across.low_01pct_fps.mean, across.low_01pct_fps.sd, 1),
            spread(across.p95_ms.mean, across.p95_ms.sd, 3),
            spread(across.p99_ms.mean, across.p99_ms.sd, 3),
            condition
                .capture
                .mean_taken_fps
                .map_or_else(|| "-".to_owned(), |fps| format!("{fps:.1}")),
            condition
                .capture
                .mean_thread_cpu_p50_us
                .map_or_else(|| "-".to_owned(), |cpu| format!("{cpu:.0} us")),
            condition
                .capture
                .mean_process_cpu_percent
                .map_or_else(|| "-".to_owned(), |cpu| format!("{cpu:.2}%")),
        );
    }
    for condition in conditions
        .iter()
        .filter(|condition| !condition.comparisons.is_empty())
    {
        println!();
        println!(
            "{} vs baseline (pool drops {}, access lost {}):",
            condition.condition,
            condition.capture.total_pool_dropped,
            condition.capture.total_access_lost
        );
        for comparison in &condition.comparisons {
            let interval = match (comparison.ci95_low_percent, comparison.ci95_high_percent) {
                (Some(low), Some(high)) => format!("[{low:+.2}%, {high:+.2}%]"),
                _ => "n/a".to_owned(),
            };
            println!(
                "  {:<14} {:+7.2}%  CI95 {:<22} noise {:.2}%  -> {:?}",
                comparison.metric,
                comparison.delta_percent,
                interval,
                comparison.baseline_noise_percent,
                comparison.verdict
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_names_are_parsed() {
        assert_eq!(parse_run_name("run-3-wgc.csv"), Some((3, "wgc".to_owned())));
        assert_eq!(
            parse_run_name("run-12-no_capture.csv"),
            Some((12, "no_capture".to_owned()))
        );
        assert_eq!(parse_run_name("run-1.csv"), None);
        assert_eq!(parse_run_name("run-x-wgc.csv"), None);
        assert_eq!(parse_run_name("run-1-.csv"), None);
        assert_eq!(parse_run_name("capture-1-wgc.json"), None);
    }

    #[test]
    fn t_table_values() {
        assert_eq!(t_critical_95(1.0), 12.706);
        assert_eq!(t_critical_95(4.9), 2.776);
        assert_eq!(t_critical_95(30.0), 2.042);
        assert_eq!(t_critical_95(45.0), 2.021);
        assert_eq!(t_critical_95(1_000.0), 1.980);
        assert_eq!(t_critical_95(0.3), 12.706);
    }

    #[test]
    fn welch_interval_matches_hand_computation() {
        // Means 12 and 15, variances 4 and 4, n = 3: se = 1.633, df = 4, t = 2.776.
        let (difference, low, high) =
            welch_interval(&[10.0, 12.0, 14.0], &[13.0, 15.0, 17.0]).unwrap();
        assert!((difference - 3.0).abs() < 1e-12);
        let margin = 2.776 * (8.0_f64 / 3.0).sqrt();
        assert!((low - (3.0 - margin)).abs() < 1e-9);
        assert!((high - (3.0 + margin)).abs() < 1e-9);
        assert!(welch_interval(&[1.0], &[1.0, 2.0]).is_none());
    }

    #[test]
    fn verdicts() {
        // Overlapping groups: interval includes zero.
        let overlapping = compare(
            "avg_fps",
            true,
            &[10.0, 12.0, 14.0],
            &[13.0, 15.0, 17.0],
            1.0,
        );
        assert_eq!(overlapping.verdict, Verdict::NoMeasurableDifference);
        assert!((overlapping.delta_percent - 25.0).abs() < 1e-9);

        // A clear 10% drop in FPS, noise 1%: worse.
        let drop = compare(
            "avg_fps",
            true,
            &[100.0, 101.0, 99.0],
            &[90.0, 91.0, 89.0],
            1.0,
        );
        assert_eq!(drop.verdict, Verdict::Worse);
        assert!((drop.delta_percent + 10.0).abs() < 1e-9);

        // Same drop on a lower-is-better metric reads as better.
        let faster = compare(
            "p99_ms",
            false,
            &[100.0, 101.0, 99.0],
            &[90.0, 91.0, 89.0],
            1.0,
        );
        assert_eq!(faster.verdict, Verdict::Better);

        // Significant but smaller than the noise floor.
        let small = compare(
            "avg_fps",
            true,
            &[100.0, 100.1, 99.9],
            &[99.0, 99.1, 98.9],
            2.0,
        );
        assert_eq!(small.verdict, Verdict::WithinNoise);

        let single = compare("avg_fps", true, &[100.0], &[90.0, 91.0], 1.0);
        assert_eq!(single.verdict, Verdict::InsufficientData);
    }
}
