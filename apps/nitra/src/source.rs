//! The host's synthetic source: a window showing stamped frames (render time
//! and id coded in the picture), captured like any other window. Each
//! viewer can read the stamp back from what it decodes; QPC is the same
//! clock in every process on the PC, so latency needs no clock sync.
//! (Same idea as `bench loopback`'s source window.)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use capture::synthetic::{Pacer, PatternMode, SyntheticConfig, SyntheticSource};
use gpu::D3d11Device;
use gpu::convert::{ConvertConfig, Converter, ShaderConverter};
use present::{FrameMeta, PresenterOptions, VideoPresenter};
use telemetry::qpc_now;

pub struct SourceWindow {
    pub hwnd: isize,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl SourceWindow {
    /// Opens the window on its own D3D11 device: the capture/encode thread
    /// uses the host device's immediate context with its own shaders, and
    /// two threads setting pipeline state on one context corrupt each other's
    /// frames (the picture flickers).
    pub fn open(adapter: &gpu::Adapter, width: u32, height: u32, fps: u32) -> Result<Self, String> {
        let device = gpu::create_device(adapter, gpu::DebugLayer::Off)
            .map_err(|e| format!("source device: {e}"))?;
        let device = &device;
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, window) = mpsc::sync_channel(1);
        let (device, thread_stop) = (device.clone(), Arc::clone(&stop));
        let thread = std::thread::Builder::new()
            .name("synthetic-source".into())
            .spawn(move || run(&device, width, height, fps, &thread_stop, &ready))
            .map_err(|e| e.to_string())?;
        let hwnd = window
            .recv()
            .map_err(|_| "source thread exited".to_owned())??;
        Ok(Self {
            hwnd,
            stop,
            thread: Some(thread),
        })
    }

    /// The window was closed (e.g. Alt+F4) or failed.
    pub fn is_closed(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn close(mut self) -> Result<(), String> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().map_or(Ok(()), |t| {
            t.join()
                .unwrap_or_else(|_| Err("source thread panicked".into()))
        })
    }
}

impl Drop for SourceWindow {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    device: &D3d11Device,
    width: u32,
    height: u32,
    fps: u32,
    stop: &AtomicBool,
    ready: &mpsc::SyncSender<Result<isize, String>>,
) -> Result<(), String> {
    let started = (|| -> Result<_, String> {
        let source = SyntheticSource::new(
            device,
            SyntheticConfig {
                width,
                height,
                mode: PatternMode::Motion,
            },
        )
        .map_err(|e| format!("synthetic source: {e}"))?;
        let converter = ShaderConverter::new(
            device,
            ConvertConfig {
                input_width: width,
                input_height: height,
                output_width: width,
                output_height: height,
                pool_size: 4,
            },
        )
        .map_err(|e| format!("converter: {e}"))?;
        let presenter = VideoPresenter::open(
            device,
            PresenterOptions {
                title: "nitra synthetic source".to_owned(),
                width,
                height,
                borderless: true,
                overlay: false,
                parent: None,
                // Not exactly over a monitor: a borderless window covering a
                // whole monitor looks like a fullscreen game to Windows, which
                // then runs that display (and the cursor) at the window's 60 Hz.
                position: Some((64, 64)),
                background: true,
                skip_when_hidden: false,
            },
        )
        .map_err(|e| format!("source window: {e}"))?;
        Ok((source, converter, presenter))
    })();
    let (mut source, mut converter, mut presenter) = match started {
        Ok(parts) => parts,
        Err(message) => {
            let _ = ready.send(Err(message.clone()));
            return Err(message);
        }
    };
    let _ = ready.send(Ok(presenter.hwnd().0 as isize));
    let mut pacer = Pacer::start_now(fps);
    while !stop.load(Ordering::Relaxed) && !presenter.is_closed() {
        let slot = pacer.next_slot(qpc_now());
        pacer.wait(&slot);
        let frame = source.render_next().map_err(|e| e.to_string())?;
        let Some(nv12) = converter
            .convert(&frame.texture)
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        presenter
            .present(
                &nv12,
                0,
                width,
                height,
                FrameMeta {
                    frame_id: u64::from(frame.stamp.frame_id),
                    captured_at: frame.stamp.timestamp,
                    decoded_at: frame.stamp.timestamp,
                    media_time: None,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    presenter.close().error.map_or(Ok(()), Err)
}
