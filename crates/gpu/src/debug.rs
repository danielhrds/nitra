//! Debug-layer helpers: D3D11 message retrieval and DXGI live-object reports.

use windows::Win32::Graphics::Direct3D11::{
    D3D11_MESSAGE, D3D11_MESSAGE_SEVERITY, D3D11_MESSAGE_SEVERITY_CORRUPTION,
    D3D11_MESSAGE_SEVERITY_ERROR, D3D11_MESSAGE_SEVERITY_INFO, D3D11_MESSAGE_SEVERITY_WARNING,
    ID3D11Device, ID3D11InfoQueue,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_DEBUG_ALL, DXGI_DEBUG_RLO_DETAIL, DXGI_DEBUG_RLO_FLAGS, DXGI_DEBUG_RLO_IGNORE_INTERNAL,
    DXGI_INFO_QUEUE_MESSAGE, DXGIGetDebugInterface1, IDXGIDebug1, IDXGIInfoQueue,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DebugSeverity {
    Corruption,
    Error,
    Warning,
    Info,
    Message,
}

impl DebugSeverity {
    fn from_d3d11(severity: D3D11_MESSAGE_SEVERITY) -> Self {
        match severity {
            D3D11_MESSAGE_SEVERITY_CORRUPTION => DebugSeverity::Corruption,
            D3D11_MESSAGE_SEVERITY_ERROR => DebugSeverity::Error,
            D3D11_MESSAGE_SEVERITY_WARNING => DebugSeverity::Warning,
            D3D11_MESSAGE_SEVERITY_INFO => DebugSeverity::Info,
            _ => DebugSeverity::Message,
        }
    }

    /// Corruption, errors and warnings: what tests should treat as failures.
    pub fn is_problem(self) -> bool {
        self <= DebugSeverity::Warning
    }
}

/// One message from the D3D11 debug layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugMessage {
    pub severity: DebugSeverity,
    pub text: String,
}

/// Removes and returns the messages stored in the device's info queue.
/// Returns nothing if the device was created without the debug layer.
pub(crate) fn drain_d3d11_messages(device: &ID3D11Device) -> Vec<DebugMessage> {
    use windows::core::Interface;
    let Ok(queue) = device.cast::<ID3D11InfoQueue>() else {
        return Vec::new();
    };
    // SAFETY: valid info queue.
    let count = unsafe { queue.GetNumStoredMessages() };
    let messages = (0..count)
        .filter_map(|index| stored_d3d11_message(&queue, index))
        .collect();
    // SAFETY: valid info queue.
    unsafe { queue.ClearStoredMessages() };
    messages
}

fn stored_d3d11_message(queue: &ID3D11InfoQueue, index: u64) -> Option<DebugMessage> {
    let mut length = 0_usize;
    // SAFETY: a null message pointer asks only for the required byte length.
    unsafe { queue.GetMessage(index, None, &mut length) }.ok()?;

    // u64 elements keep the buffer aligned for D3D11_MESSAGE.
    let mut buffer = vec![0_u64; length.div_ceil(size_of::<u64>())];
    let message = buffer.as_mut_ptr().cast::<D3D11_MESSAGE>();
    // SAFETY: `buffer` holds at least `length` bytes, suitably aligned.
    unsafe { queue.GetMessage(index, Some(message), &mut length) }.ok()?;

    // SAFETY: on success the buffer starts with an initialized message whose
    // description points to `DescriptionByteLength` bytes inside `buffer`.
    let (severity, description) = unsafe {
        let message = &*message;
        (
            message.Severity,
            std::slice::from_raw_parts(message.pDescription, message.DescriptionByteLength),
        )
    };
    Some(DebugMessage {
        severity: DebugSeverity::from_d3d11(severity),
        text: String::from_utf8_lossy(description)
            .trim_end_matches('\0')
            .to_owned(),
    })
}

/// Asks DXGI which objects are still alive and returns its messages.
///
/// Call it after every device, factory and resource has been released: an
/// empty list then means nothing leaked. Returns `None` when the debug
/// interfaces are unavailable (Windows "Graphics Tools" not installed).
pub fn report_live_objects() -> Option<Vec<String>> {
    // SAFETY: the flags argument must be zero; the interfaces are returned as owned COM references.
    let debug: IDXGIDebug1 = unsafe { DXGIGetDebugInterface1(0) }.ok()?;
    // SAFETY: as above.
    let queue: IDXGIInfoQueue = unsafe { DXGIGetDebugInterface1(0) }.ok()?;

    let flags = DXGI_DEBUG_RLO_FLAGS(DXGI_DEBUG_RLO_DETAIL.0 | DXGI_DEBUG_RLO_IGNORE_INTERNAL.0);
    // SAFETY: valid debug interfaces; the producer id is a static GUID.
    unsafe {
        queue.ClearStoredMessages(DXGI_DEBUG_ALL);
        debug.ReportLiveObjects(DXGI_DEBUG_ALL, flags).ok()?;
    }

    // SAFETY: valid queue.
    let count = unsafe { queue.GetNumStoredMessages(DXGI_DEBUG_ALL) };
    (0..count)
        .map(|index| stored_message(&queue, index))
        .collect()
}

fn stored_message(queue: &IDXGIInfoQueue, index: u64) -> Option<String> {
    let mut length = 0_usize;
    // SAFETY: a null message pointer asks only for the required byte length.
    unsafe { queue.GetMessage(DXGI_DEBUG_ALL, index, None, &mut length) }.ok()?;

    // u64 elements keep the buffer aligned for DXGI_INFO_QUEUE_MESSAGE.
    let mut buffer = vec![0_u64; length.div_ceil(size_of::<u64>())];
    let message = buffer.as_mut_ptr().cast::<DXGI_INFO_QUEUE_MESSAGE>();
    // SAFETY: `buffer` holds at least `length` bytes, suitably aligned.
    unsafe { queue.GetMessage(DXGI_DEBUG_ALL, index, Some(message), &mut length) }.ok()?;

    // SAFETY: on success the buffer starts with an initialized message whose
    // description points to `DescriptionByteLength` bytes inside `buffer`.
    let description = unsafe {
        let message = &*message;
        std::slice::from_raw_parts(message.pDescription, message.DescriptionByteLength)
    };
    Some(
        String::from_utf8_lossy(description)
            .trim_end_matches('\0')
            .to_owned(),
    )
}
