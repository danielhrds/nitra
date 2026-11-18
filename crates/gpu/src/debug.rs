//! Live-object reporting through the DXGI debug interfaces.

use windows::Win32::Graphics::Dxgi::{
    DXGI_DEBUG_ALL, DXGI_DEBUG_RLO_DETAIL, DXGI_DEBUG_RLO_FLAGS, DXGI_DEBUG_RLO_IGNORE_INTERNAL,
    DXGI_INFO_QUEUE_MESSAGE, DXGIGetDebugInterface1, IDXGIDebug1, IDXGIInfoQueue,
};

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
