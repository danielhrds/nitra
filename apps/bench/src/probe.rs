//! `bench probe`: read-only report of adapters, outputs, capture support and
//! hardware codecs. Starts no capture, encoder or decoder.

use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use capture::probe::{DuplicationSupport, WgcSupport, dxgi_duplication_support, wgc_support};
use decode::probe::{D3d11DecodeSupport, d3d11_decode_support, h264_decoder_mfts};
use encode::probe::{AmfRuntime, amf_runtime, h264_hardware_encoders};
use gpu::{AdapterInfo, DebugLayer, OutputInfo};
use mf::{MediaFoundation, MftInfo};
use serde::Serialize;

use crate::com::ComApartment;
use crate::os_info::OsInfo;
use crate::process_stats::ProcessStats;

/// Bumped whenever a field is renamed, removed or changes meaning.
pub const PROBE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub schema_version: u32,
    pub app_version: &'static str,
    pub build_profile: &'static str,
    pub created_unix_ms: u64,
    pub os: OsInfo,
    pub adapters: Vec<AdapterReport>,
    pub wgc: Option<WgcSupport>,
    pub h264_hardware_encoders: Vec<MftInfo>,
    pub h264_decoder_mfts: Vec<MftInfo>,
    pub amf: AmfRuntime,
    /// Probe steps that failed; everything else in the report is still valid.
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AdapterReport {
    #[serde(flatten)]
    pub info: AdapterInfo,
    pub outputs: Vec<OutputReport>,
    /// `None` for software adapters or when device creation failed.
    pub d3d11: Option<DeviceReport>,
}

#[derive(Debug, Serialize)]
pub struct OutputReport {
    #[serde(flatten)]
    pub info: OutputInfo,
    pub dxgi_duplication: DuplicationSupport,
}

#[derive(Debug, Serialize)]
pub struct DeviceReport {
    pub feature_level: &'static str,
    pub debug_layer: bool,
    pub decode: Option<D3d11DecodeSupport>,
}

/// Runs the probe `repeat` times, prints the last report and writes it as JSON.
pub fn run(json: Option<PathBuf>, repeat: u32) -> Result<(), Box<dyn Error>> {
    let com = ComApartment::multithreaded()?;
    let media_foundation = MediaFoundation::start()?;

    let mut report = collect();
    let after_first = ProcessStats::current()?;
    for _ in 1..repeat {
        report = collect();
    }
    let after_last = ProcessStats::current()?;

    print!("{}", render_text(&report));
    if repeat > 1 {
        println!();
        println!("Leak check ({repeat} runs; the first run loads DLLs and is the baseline):");
        println!(
            "  after run 1:  {} handles, {:.1} MiB private",
            after_first.handles,
            mib(after_first.private_bytes)
        );
        println!(
            "  after run {repeat}: {} handles, {:.1} MiB private",
            after_last.handles,
            mib(after_last.private_bytes)
        );
        println!(
            "  growth:       {:+} handles, {:+.1} MiB private",
            i64::from(after_last.handles) - i64::from(after_first.handles),
            mib(after_last.private_bytes) - mib(after_first.private_bytes)
        );
    }

    let path = json.unwrap_or_else(|| {
        telemetry::default_output_dir().join(format!("probe-{}.json", report.created_unix_ms))
    });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(&report)?)?;
    println!();
    println!("JSON: {}", path.display());

    // Every COM object from the probe is released by now (the report holds
    // plain data only), so DXGI should report no live objects.
    drop(media_foundation);
    if cfg!(debug_assertions) {
        match gpu::report_live_objects() {
            None => {
                println!("DXGI live objects: not checked (Windows Graphics Tools not installed)")
            }
            Some(messages) if messages.is_empty() => println!("DXGI live objects: none"),
            Some(messages) => {
                println!("DXGI live objects: {} message(s)", messages.len());
                for message in messages {
                    println!("  {message}");
                }
            }
        }
    }
    drop(com);
    Ok(())
}

fn collect() -> ProbeReport {
    let mut errors = Vec::new();
    let debug_layer = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };

    let adapters = record(&mut errors, "enumerate adapters", gpu::enumerate_adapters())
        .unwrap_or_default()
        .iter()
        .map(|adapter| adapter_report(adapter, debug_layer, &mut errors))
        .collect();
    let wgc = record(&mut errors, "WGC support", wgc_support());
    let h264_hardware_encoders = record(
        &mut errors,
        "H.264 hardware encoders",
        h264_hardware_encoders(),
    )
    .unwrap_or_default();
    let h264_decoder_mfts =
        record(&mut errors, "H.264 decoder MFTs", h264_decoder_mfts()).unwrap_or_default();

    ProbeReport {
        schema_version: PROBE_SCHEMA_VERSION,
        app_version: env!("CARGO_PKG_VERSION"),
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        created_unix_ms: unix_millis(),
        os: OsInfo::read(),
        adapters,
        wgc,
        h264_hardware_encoders,
        h264_decoder_mfts,
        amf: amf_runtime(),
        errors,
    }
}

fn adapter_report(
    adapter: &gpu::Adapter,
    debug_layer: DebugLayer,
    errors: &mut Vec<String>,
) -> AdapterReport {
    let outputs = adapter
        .outputs
        .iter()
        .map(|output| OutputReport {
            info: output.info.clone(),
            dxgi_duplication: dxgi_duplication_support(output.handle()),
        })
        .collect();

    let d3d11 = if adapter.info.is_software {
        None
    } else {
        let step = format!("D3D11 device on adapter {}", adapter.info.index);
        record(errors, &step, gpu::create_device(adapter, debug_layer)).map(|device| {
            let step = format!("D3D11 decode profiles on adapter {}", adapter.info.index);
            DeviceReport {
                feature_level: device.feature_level_name(),
                debug_layer: device.debug_layer_enabled(),
                decode: record(errors, &step, d3d11_decode_support(&device)),
            }
        })
    };

    AdapterReport {
        info: adapter.info.clone(),
        outputs,
        d3d11,
    }
}

/// Keeps the value on success; on failure notes which step failed and why.
fn record<T>(errors: &mut Vec<String>, step: &str, result: windows::core::Result<T>) -> Option<T> {
    result
        .map_err(|error| {
            tracing::warn!(step, %error, "probe step failed");
            errors.push(format!("{step}: {error}"));
        })
        .ok()
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_millis() as u64)
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn optional<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "?".to_owned(), |value| value.to_string())
}

/// Human-readable rendering of a report.
fn render_text(report: &ProbeReport) -> String {
    let mut text = String::new();
    // Writing to a String cannot fail, so the fmt::Results below are ignored.
    let _ = writeln!(
        text,
        "ziz probe {} ({})",
        report.app_version, report.build_profile
    );
    let _ = writeln!(
        text,
        "OS: Windows {} {}, build {}",
        optional(report.os.edition.as_deref()),
        optional(report.os.display_version.as_deref()),
        optional(report.os.build.as_deref())
    );

    for adapter in &report.adapters {
        render_adapter(&mut text, adapter);
    }

    let _ = writeln!(text);
    match &report.wgc {
        Some(wgc) => {
            let _ = writeln!(
                text,
                "WGC: supported {}; border toggle {}, cursor toggle {}, min update interval {}",
                yes_no(wgc.supported),
                yes_no(wgc.border_toggle),
                yes_no(wgc.cursor_toggle),
                yes_no(wgc.min_update_interval)
            );
        }
        None => {
            let _ = writeln!(text, "WGC: unknown (see errors)");
        }
    }

    let _ = writeln!(text, "H.264 hardware encoders (Media Foundation):");
    render_mfts(&mut text, &report.h264_hardware_encoders);
    let _ = writeln!(text, "H.264 decoder MFTs (Media Foundation):");
    render_mfts(&mut text, &report.h264_decoder_mfts);

    let amf = match &report.amf {
        AmfRuntime::NotInstalled => "not installed".to_owned(),
        AmfRuntime::Present { version } => format!("present, version {version}"),
        AmfRuntime::QueryFailed { detail } => format!("query failed ({detail})"),
    };
    let _ = writeln!(text, "AMF runtime: {amf}");

    if !report.errors.is_empty() {
        let _ = writeln!(text, "Errors:");
        for error in &report.errors {
            let _ = writeln!(text, "  - {error}");
        }
    }
    text
}

fn render_adapter(text: &mut String, adapter: &AdapterReport) {
    let info = &adapter.info;
    let _ = writeln!(text);
    let _ = writeln!(
        text,
        "Adapter {}: {} [{} {:04X}:{:04X}]{}",
        info.index,
        info.name,
        info.vendor.name(),
        info.vendor_id,
        info.device_id,
        if info.is_software { " (software)" } else { "" }
    );
    let _ = writeln!(
        text,
        "  LUID {}, dedicated VRAM {:.2} GiB, driver {}",
        info.luid,
        gib(info.dedicated_video_memory_bytes),
        optional(info.driver_version.as_deref())
    );

    if let Some(device) = &adapter.d3d11 {
        let _ = writeln!(
            text,
            "  D3D11: feature level {}, debug layer {}",
            device.feature_level,
            if device.debug_layer { "on" } else { "off" }
        );
        if let Some(decode) = &device.decode {
            let named: Vec<&str> = decode
                .profiles
                .iter()
                .filter_map(|profile| profile.name)
                .collect();
            let unnamed = decode.profiles.len() - named.len();
            let _ = writeln!(
                text,
                "  D3D11 decode: H.264 to NV12 {}; profiles: {} (+{unnamed} other)",
                yes_no(decode.h264_nv12),
                named.join(", ")
            );
        }
    }

    for output in &adapter.outputs {
        let info = &output.info;
        let mode = info.current_mode.map_or_else(
            || "mode unknown".to_owned(),
            |mode| {
                format!(
                    "{}x{} @ {} Hz",
                    mode.width, mode.height, mode.refresh_rate_hz
                )
            },
        );
        let desktop = info.desktop;
        let _ = writeln!(
            text,
            "  Output {} {}: {mode}, desktop ({},{})-({},{}), {}, {} {}-bit, HDR {}",
            info.index,
            info.device_name,
            desktop.left,
            desktop.top,
            desktop.right,
            desktop.bottom,
            info.rotation,
            optional(info.color_space),
            optional(info.bits_per_color),
            info.hdr_active
                .map_or("?", |hdr| if hdr { "on" } else { "off" })
        );
        let _ = writeln!(
            text,
            "    DXGI duplication: DuplicateOutput {}, DuplicateOutput1 {}",
            yes_no(output.dxgi_duplication.output1),
            yes_no(output.dxgi_duplication.output5)
        );
    }
}

fn render_mfts(text: &mut String, mfts: &[MftInfo]) {
    if mfts.is_empty() {
        let _ = writeln!(text, "  (none)");
    }
    for mft in mfts {
        let _ = writeln!(
            text,
            "  - {} [{}] {}, {}",
            mft.name.as_deref().unwrap_or("(unnamed)"),
            mft.hardware_vendor_id.as_deref().unwrap_or("no vendor id"),
            if mft.hardware { "hardware" } else { "software" },
            if mft.asynchronous { "async" } else { "sync" }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpu::{AdapterLuid, DesktopRect, DisplayMode, Vendor};

    fn sample_report() -> ProbeReport {
        ProbeReport {
            schema_version: PROBE_SCHEMA_VERSION,
            app_version: "0.1.0",
            build_profile: "debug",
            created_unix_ms: 1,
            os: OsInfo {
                edition: Some("Professional".to_owned()),
                display_version: Some("25H2".to_owned()),
                build: Some("26200.1".to_owned()),
            },
            adapters: vec![AdapterReport {
                info: AdapterInfo {
                    index: 0,
                    name: "Test GPU".to_owned(),
                    vendor: Vendor::Amd,
                    vendor_id: 0x1002,
                    device_id: 0x7480,
                    luid: AdapterLuid { high: 0, low: 42 },
                    dedicated_video_memory_bytes: 8 << 30,
                    shared_system_memory_bytes: 0,
                    is_software: false,
                    driver_version: Some("32.0.31021.5001".to_owned()),
                },
                outputs: vec![OutputReport {
                    info: OutputInfo {
                        index: 0,
                        device_name: r"\\.\DISPLAY1".to_owned(),
                        attached_to_desktop: true,
                        desktop: DesktopRect {
                            left: 0,
                            top: 0,
                            right: 2560,
                            bottom: 1440,
                        },
                        rotation: "identity",
                        current_mode: Some(DisplayMode {
                            width: 2560,
                            height: 1440,
                            refresh_rate_hz: 165,
                        }),
                        bits_per_color: Some(8),
                        color_space: Some("srgb"),
                        hdr_active: Some(false),
                        max_luminance_nits: Some(270.0),
                    },
                    dxgi_duplication: DuplicationSupport {
                        output1: true,
                        output5: true,
                    },
                }],
                d3d11: Some(DeviceReport {
                    feature_level: "11_1",
                    debug_layer: false,
                    decode: None,
                }),
            }],
            wgc: Some(WgcSupport {
                supported: true,
                border_toggle: true,
                cursor_toggle: true,
                min_update_interval: false,
            }),
            h264_hardware_encoders: vec![MftInfo {
                name: Some("Test H264 Encoder".to_owned()),
                clsid: None,
                hardware: true,
                asynchronous: true,
                hardware_vendor_id: Some("VEN_1002".to_owned()),
            }],
            h264_decoder_mfts: Vec::new(),
            amf: AmfRuntime::NotInstalled,
            errors: vec!["example step: failed".to_owned()],
        }
    }

    #[test]
    fn text_mentions_every_section() {
        let text = render_text(&sample_report());
        for expected in [
            "OS: Windows Professional 25H2, build 26200.1",
            "Adapter 0: Test GPU [AMD 1002:7480]",
            "LUID 00000000-0000002A, dedicated VRAM 8.00 GiB, driver 32.0.31021.5001",
            "D3D11: feature level 11_1, debug layer off",
            r"Output 0 \\.\DISPLAY1: 2560x1440 @ 165 Hz",
            "DuplicateOutput yes, DuplicateOutput1 yes",
            "WGC: supported yes; border toggle yes, cursor toggle yes, min update interval no",
            "- Test H264 Encoder [VEN_1002] hardware, async",
            "(none)",
            "AMF runtime: not installed",
            "- example step: failed",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    #[test]
    fn json_flattens_adapter_and_output_fields() {
        let json = serde_json::to_value(sample_report()).unwrap();
        let adapter = &json["adapters"][0];
        assert_eq!(json["schema_version"], PROBE_SCHEMA_VERSION);
        assert_eq!(adapter["name"], "Test GPU");
        assert_eq!(adapter["vendor"], "amd");
        assert_eq!(adapter["luid"], "00000000-0000002A");
        assert_eq!(adapter["d3d11"]["feature_level"], "11_1");
        assert_eq!(adapter["outputs"][0]["device_name"], r"\\.\DISPLAY1");
        assert_eq!(adapter["outputs"][0]["dxgi_duplication"]["output5"], true);
        assert_eq!(json["amf"]["status"], "not_installed");
        assert_eq!(json["os"]["build"], "26200.1");
    }

    #[test]
    fn record_keeps_values_and_collects_errors() {
        let mut errors = Vec::new();
        assert_eq!(record(&mut errors, "ok step", Ok(5)), Some(5));
        let failed: Option<()> = record(
            &mut errors,
            "failing step",
            Err(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            )),
        );
        assert_eq!(failed, None);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("failing step: "), "{}", errors[0]);
    }
}
