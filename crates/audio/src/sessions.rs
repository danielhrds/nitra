//! Which processes are playing sound: the audio sessions of the default
//! output device, plus a Windows notification (an event we wait on) when a
//! new session appears, so a newly playing app joins the mix at once.
//! Create and use it on a COM (MTA) thread.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    AudioSessionStateExpired, IAudioSessionControl, IAudioSessionControl2, IAudioSessionManager2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDeviceEnumerator,
    MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Threading::SetEvent;
use windows::core::{Interface, Ref};
use windows_core::implement;

use crate::capture::Event;

/// Signals the event when Windows reports a new session (any thread).
#[implement(IAudioSessionNotification)]
struct SessionCreated(HANDLE);

impl IAudioSessionNotification_Impl for SessionCreated_Impl {
    fn OnSessionCreated(&self, _session: Ref<IAudioSessionControl>) -> windows::core::Result<()> {
        // SAFETY: the event outlives the registration (unregistered before
        // the event is closed, in `Drop`).
        unsafe { SetEvent(self.0) }
    }
}

pub struct SessionWatcher {
    manager: IAudioSessionManager2,
    notification: IAudioSessionNotification,
    changed: Event,
}

impl SessionWatcher {
    pub fn new() -> Result<Self, String> {
        let changed = Event::new()?;
        // SAFETY: COM is initialized on this thread (caller's contract); the
        // calls return owned interfaces.
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| format!("devices: {e}"))?;
            let device = devices
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| format!("default output: {e}"))?;
            let manager: IAudioSessionManager2 = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("session manager: {e}"))?;
            // Windows only starts notifying after the session list was asked
            // for once.
            let _ = manager.GetSessionEnumerator();
            let notification: IAudioSessionNotification = SessionCreated(changed.0).into();
            manager
                .RegisterSessionNotification(&notification)
                .map_err(|e| format!("session notification: {e}"))?;
            Ok(Self {
                manager,
                notification,
                changed,
            })
        }
    }

    /// Signaled when a new session appears.
    pub fn changed_event(&self) -> HANDLE {
        self.changed.0
    }

    /// Process ids of the sessions that are not expired (0 is the system
    /// sounds session).
    pub fn sounding(&self) -> Result<Vec<u32>, String> {
        let mut pids = Vec::new();
        // SAFETY: valid manager; every interface returned is owned.
        unsafe {
            let sessions = self
                .manager
                .GetSessionEnumerator()
                .map_err(|e| e.to_string())?;
            for i in 0..sessions.GetCount().map_err(|e| e.to_string())? {
                let Ok(session) = sessions.GetSession(i) else {
                    continue;
                };
                if session
                    .GetState()
                    .is_ok_and(|s| s == AudioSessionStateExpired)
                {
                    continue;
                }
                if let Ok(pid) = session
                    .cast::<IAudioSessionControl2>()
                    .and_then(|s| s.GetProcessId())
                {
                    pids.push(pid);
                }
            }
        }
        pids.sort_unstable();
        pids.dedup();
        Ok(pids)
    }
}

impl Drop for SessionWatcher {
    fn drop(&mut self) {
        // SAFETY: unregisters what `new` registered, before `changed` closes.
        let _ = unsafe {
            self.manager
                .UnregisterSessionNotification(&self.notification)
        };
    }
}
