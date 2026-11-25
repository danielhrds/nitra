//! The GPU nitra runs on, and whether nitra was tested on it. The window, the
//! share and the watched screens all use the first hardware adapter.
//! Sharing was validated on AMD (AMF, Radeon RX 7600). NVIDIA and Intel
//! encode with the Media Foundation encoder their drivers install (NVENC,
//! Quick Sync): the same path other apps use, but not yet tried with nitra,
//! so sharing there is experimental and asks first. Watching uses the
//! Windows H.264 decoder (DXVA) on every vendor.

use gpu::{Adapter, AdapterInfo, Vendor};

use crate::i18n::tr;

/// The adapter everything runs on: the first one that is not a software
/// rasterizer. On a laptop with two GPUs this can be the integrated one
/// while a game runs on the other (a known limit: not tested).
pub fn main_adapter(adapters: &[Adapter]) -> Option<&Adapter> {
    adapters.iter().find(|a| !a.info.is_software)
}

/// Whether nitra was tested on a GPU vendor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Tested,
    Experimental,
}

impl Support {
    pub fn of(vendor: Vendor) -> Self {
        match vendor {
            Vendor::Amd => Self::Tested,
            Vendor::Nvidia | Vendor::Intel | Vendor::Microsoft | Vendor::Other => {
                Self::Experimental
            }
        }
    }

    /// As the settings show it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tested => tr("testada com o nitra", "tested with nitra"),
            Self::Experimental => tr(
                "experimental (ainda não testada com o nitra)",
                "experimental (not tested with nitra yet)",
            ),
        }
    }
}

/// The GPU as the window shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gpu {
    pub name: String,
    pub support: Support,
}

impl Gpu {
    pub fn of(info: &AdapterInfo) -> Self {
        // Testing: `NITRA_TEST_VENDOR=nvidia` (or `intel`) makes the window
        // act as on that vendor, to see what its users see. Unset in normal
        // use; it changes only what the window shows and asks, never which
        // encoder runs.
        if let Some(vendor) = std::env::var("NITRA_TEST_VENDOR")
            .ok()
            .and_then(|name| test_vendor(&name))
        {
            return Self {
                name: format!(
                    "{} ({})",
                    vendor.name(),
                    tr("simulada para teste", "simulated for testing")
                ),
                support: Support::of(vendor),
            };
        }
        Self {
            name: info.name.trim().to_owned(),
            support: Support::of(info.vendor),
        }
    }
}

fn test_vendor(name: &str) -> Option<Vendor> {
    match name.to_ascii_lowercase().as_str() {
        "amd" => Some(Vendor::Amd),
        "nvidia" => Some(Vendor::Nvidia),
        "intel" => Some(Vendor::Intel),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_amd_is_tested() {
        assert_eq!(Support::of(Vendor::Amd), Support::Tested);
        for vendor in [
            Vendor::Nvidia,
            Vendor::Intel,
            Vendor::Microsoft,
            Vendor::Other,
        ] {
            assert_eq!(Support::of(vendor), Support::Experimental, "{vendor:?}");
        }
    }

    #[test]
    fn pci_ids_map_to_their_support() {
        // AMD's two PCI vendor ids, NVIDIA, Intel.
        assert_eq!(Support::of(Vendor::from_pci_id(0x1002)), Support::Tested);
        assert_eq!(Support::of(Vendor::from_pci_id(0x1022)), Support::Tested);
        assert_eq!(
            Support::of(Vendor::from_pci_id(0x10DE)),
            Support::Experimental
        );
        assert_eq!(
            Support::of(Vendor::from_pci_id(0x8086)),
            Support::Experimental
        );
    }

    #[test]
    fn test_vendor_names() {
        assert_eq!(test_vendor("NVIDIA"), Some(Vendor::Nvidia));
        assert_eq!(test_vendor("intel"), Some(Vendor::Intel));
        assert_eq!(test_vendor("amd"), Some(Vendor::Amd));
        assert_eq!(test_vendor("matrox"), None);
    }
}
