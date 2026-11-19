//! Picks the monitor capture API: WGC by default (the same game impact at
//! a third of the CPU), DXGI Desktop Duplication when WGC cannot start.

use std::fmt;

use gpu::{D3d11Device, Output};
use serde::Serialize;

use crate::common::{Capture, CaptureOptions};
use crate::wgc::CaptureTarget;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Wgc,
    Dxgi,
}

impl CaptureKind {
    pub fn name(self) -> &'static str {
        match self {
            CaptureKind::Wgc => "wgc",
            CaptureKind::Dxgi => "dxgi",
        }
    }
}

impl fmt::Display for CaptureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which monitor capture API the user asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CapturePreference {
    /// WGC, falling back to DXGI.
    #[default]
    Auto,
    Wgc,
    Dxgi,
}

pub fn monitor_capture_order(preference: CapturePreference) -> Vec<CaptureKind> {
    match preference {
        CapturePreference::Auto => vec![CaptureKind::Wgc, CaptureKind::Dxgi],
        CapturePreference::Wgc => vec![CaptureKind::Wgc],
        CapturePreference::Dxgi => vec![CaptureKind::Dxgi],
    }
}

/// A capture API that could not start, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FailedCapture {
    pub kind: CaptureKind,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CaptureSelection {
    pub kind: CaptureKind,
    pub failed: Vec<FailedCapture>,
}

/// Tries `order` until `start` succeeds; returns the capture and how it was
/// chosen, or every failure.
pub fn first_working<T>(
    order: &[CaptureKind],
    mut start: impl FnMut(CaptureKind) -> Result<T, String>,
) -> Result<(T, CaptureSelection), Vec<FailedCapture>> {
    let mut failed = Vec::new();
    for &kind in order {
        match start(kind) {
            Ok(value) => return Ok((value, CaptureSelection { kind, failed })),
            Err(error) => failed.push(FailedCapture { kind, error }),
        }
    }
    Err(failed)
}

/// Starts capturing monitor `output` with the preferred API.
pub fn start_monitor(
    device: &D3d11Device,
    output: &Output,
    options: CaptureOptions,
    preference: CapturePreference,
) -> Result<(Capture, CaptureSelection), String> {
    first_working(&monitor_capture_order(preference), |kind| match kind {
        CaptureKind::Wgc => {
            crate::wgc::start(device, CaptureTarget::Monitor(output.monitor()), options)
        }
        CaptureKind::Dxgi => crate::dxgi::start(device, output, options),
    })
    .map_err(|failed| {
        failed
            .iter()
            .map(|attempt| format!("{}: {}", attempt.kind, attempt.error))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_tries_wgc_then_dxgi() {
        assert_eq!(
            monitor_capture_order(CapturePreference::Auto),
            [CaptureKind::Wgc, CaptureKind::Dxgi]
        );
        assert_eq!(
            monitor_capture_order(CapturePreference::Dxgi),
            [CaptureKind::Dxgi]
        );
    }

    #[test]
    fn falls_back_to_dxgi_and_records_why() {
        let (value, selection) =
            first_working(&[CaptureKind::Wgc, CaptureKind::Dxgi], |kind| match kind {
                CaptureKind::Wgc => Err("not supported".to_owned()),
                CaptureKind::Dxgi => Ok(3),
            })
            .unwrap();
        assert_eq!(value, 3);
        assert_eq!(selection.kind, CaptureKind::Dxgi);
        assert_eq!(
            selection.failed,
            [FailedCapture {
                kind: CaptureKind::Wgc,
                error: "not supported".to_owned()
            }]
        );
    }

    #[test]
    fn reports_every_failure() {
        let failed = first_working::<()>(&[CaptureKind::Wgc, CaptureKind::Dxgi], |_| {
            Err("no".to_owned())
        })
        .unwrap_err();
        assert_eq!(failed.len(), 2);
    }
}
