//! Picks the hardware encoder for an adapter: AMF on AMD
//! GPUs, Media Foundation on the others and whenever AMF cannot start.

use std::fmt;

use gpu::{AdapterInfo, D3d11Device, Vendor};
use serde::Serialize;

use crate::amf::AmfEncoder;
use crate::contract::{EncodeError, EncoderConfig, VideoEncoder};
use crate::media_foundation::MediaFoundationEncoder;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderKind {
    Amf,
    MediaFoundation,
}

impl EncoderKind {
    pub fn name(self) -> &'static str {
        match self {
            EncoderKind::Amf => "amf",
            EncoderKind::MediaFoundation => "media-foundation",
        }
    }
}

impl fmt::Display for EncoderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which encoder the user asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EncoderPreference {
    /// AMF on AMD (falling back to Media Foundation), Media Foundation elsewhere.
    #[default]
    Auto,
    Amf,
    MediaFoundation,
}

/// The encoders to try, in order.
pub fn encoder_order(vendor: Vendor, preference: EncoderPreference) -> Vec<EncoderKind> {
    match preference {
        EncoderPreference::Amf => vec![EncoderKind::Amf],
        EncoderPreference::MediaFoundation => vec![EncoderKind::MediaFoundation],
        EncoderPreference::Auto if vendor == Vendor::Amd => {
            vec![EncoderKind::Amf, EncoderKind::MediaFoundation]
        }
        EncoderPreference::Auto => vec![EncoderKind::MediaFoundation],
    }
}

/// An encoder that could not start, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FailedAttempt {
    pub kind: EncoderKind,
    pub error: String,
}

/// Which encoder runs, how it is configured and what failed before it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EncoderSelection {
    pub kind: EncoderKind,
    /// The MFT name and candidates, or the AMF runtime version.
    pub detail: String,
    pub applied_settings: String,
    pub failed: Vec<FailedAttempt>,
}

/// Tries `order` until `create` succeeds; returns the result, its kind and
/// the attempts that failed before it, or every failure.
pub fn first_working<T, E: fmt::Display>(
    order: &[EncoderKind],
    mut create: impl FnMut(EncoderKind) -> Result<T, E>,
) -> Result<(T, EncoderKind, Vec<FailedAttempt>), Vec<FailedAttempt>> {
    let mut failed = Vec::new();
    for &kind in order {
        match create(kind) {
            Ok(value) => return Ok((value, kind, failed)),
            Err(error) => failed.push(FailedAttempt {
                kind,
                error: error.to_string(),
            }),
        }
    }
    Err(failed)
}

/// Creates the encoder for `adapter` (the one `device` was created on).
/// The caller must have initialized COM (MTA) and started Media Foundation
/// on this thread.
pub fn create_encoder(
    device: &D3d11Device,
    adapter: &AdapterInfo,
    config: EncoderConfig,
    preference: EncoderPreference,
) -> Result<(Box<dyn VideoEncoder>, EncoderSelection), EncodeError> {
    let order = encoder_order(adapter.vendor, preference);
    let created = first_working(&order, |kind| -> Result<_, EncodeError> {
        Ok(match kind {
            EncoderKind::Amf => {
                let encoder = AmfEncoder::new(device, config)?;
                let detail = format!("AMF runtime {}", encoder.runtime_version());
                let applied = format!("{:?}", encoder.applied_settings());
                (Box::new(encoder) as Box<dyn VideoEncoder>, detail, applied)
            }
            EncoderKind::MediaFoundation => {
                let encoder = MediaFoundationEncoder::new(device, adapter.luid, config)?;
                let detail = format!(
                    "MFT {}, {} candidate(s)",
                    encoder.info().name.as_deref().unwrap_or("?"),
                    encoder.candidates()
                );
                let applied = format!("{:?}", encoder.applied_settings());
                (Box::new(encoder) as Box<dyn VideoEncoder>, detail, applied)
            }
        })
    });
    match created {
        Ok(((encoder, detail, applied_settings), kind, failed)) => Ok((
            encoder,
            EncoderSelection {
                kind,
                detail,
                applied_settings,
                failed,
            },
        )),
        Err(failed) => Err(EncodeError::NotFound(
            failed
                .iter()
                .map(|attempt| format!("{}: {}", attempt.kind, attempt.error))
                .collect::<Vec<_>>()
                .join("; "),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amd_prefers_amf_with_media_foundation_as_fallback() {
        assert_eq!(
            encoder_order(Vendor::Amd, EncoderPreference::Auto),
            [EncoderKind::Amf, EncoderKind::MediaFoundation]
        );
        for vendor in [Vendor::Nvidia, Vendor::Intel, Vendor::Other] {
            assert_eq!(
                encoder_order(vendor, EncoderPreference::Auto),
                [EncoderKind::MediaFoundation]
            );
        }
    }

    #[test]
    fn forced_preferences_have_no_fallback() {
        assert_eq!(
            encoder_order(Vendor::Amd, EncoderPreference::MediaFoundation),
            [EncoderKind::MediaFoundation]
        );
        assert_eq!(
            encoder_order(Vendor::Nvidia, EncoderPreference::Amf),
            [EncoderKind::Amf]
        );
    }

    #[test]
    fn falls_back_and_records_the_failure() {
        let order = [EncoderKind::Amf, EncoderKind::MediaFoundation];
        let (value, kind, failed) = first_working(&order, |kind| match kind {
            EncoderKind::Amf => Err("runtime missing"),
            EncoderKind::MediaFoundation => Ok(7),
        })
        .unwrap();
        assert_eq!((value, kind), (7, EncoderKind::MediaFoundation));
        assert_eq!(
            failed,
            [FailedAttempt {
                kind: EncoderKind::Amf,
                error: "runtime missing".to_owned()
            }]
        );
    }

    #[test]
    fn reports_every_failure_when_nothing_starts() {
        let order = [EncoderKind::Amf, EncoderKind::MediaFoundation];
        let failed = first_working(&order, |_| Err::<(), _>("no")).unwrap_err();
        assert_eq!(failed.len(), 2);
    }

    #[test]
    fn first_choice_wins_without_failures() {
        let order = [EncoderKind::Amf, EncoderKind::MediaFoundation];
        let (_, kind, failed) = first_working(&order, |_| Ok::<_, &str>(())).unwrap();
        assert_eq!(kind, EncoderKind::Amf);
        assert!(failed.is_empty());
    }
}
