//! Per-application audio capture.
//!
//! Each platform module exposes:
//!   fn list_sources() -> Result<Vec<Source>, String>
//!   fn run(src: &Source, cb: &mut AudioCallback, stop: &AtomicBool, ready: impl FnOnce(Result<(), String>)) -> Result<(), String>

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
use windows as platform;

#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    /// Platform handle: HWND on Windows, CGWindowID on macOS, sink-input index on Linux.
    pub id: u64,
    pub pid: u32,
    pub title: String,
    pub app: String,
    /// True when the source is known to be producing sound right now.
    pub audible: bool,
}

/// Receives interleaved f32 samples: (samples, channels, sample_rate).
pub type AudioCallback = Box<dyn FnMut(&[f32], usize, u32) + Send>;

pub fn list_sources() -> Result<Vec<Source>, String> {
    let mut v = platform::list_sources()?;
    v.sort_by(|a, b| {
        b.audible
            .cmp(&a.audible)
            .then_with(|| a.app.to_lowercase().cmp(&b.app.to_lowercase()))
            .then_with(|| a.title.cmp(&b.title))
    });
    Ok(v)
}

pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Starts capturing on a dedicated thread. Blocks until the stream is
    /// running (or failed to start). `on_end` is called if the stream later
    /// dies with an error.
    pub fn start(
        src: &Source,
        mut cb: AudioCallback,
        on_end: Box<dyn FnOnce(String) + Send>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let src = src.clone();
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                let tx2 = tx.clone();
                let ready = move |r: Result<(), String>| {
                    let _ = tx2.send(r);
                };
                if let Err(e) = platform::run(&src, &mut cb, &stop2, ready) {
                    // If startup already succeeded this goes nowhere; report via on_end.
                    let _ = tx.send(Err(e.clone()));
                    if !stop2.load(Ordering::Relaxed) {
                        on_end(e);
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        let mut cap = Self { stop, thread: Some(thread) };
        match rx.recv() {
            Ok(Ok(())) => Ok(cap),
            Ok(Err(e)) => {
                cap.shutdown();
                Err(e)
            }
            Err(_) => {
                cap.shutdown();
                Err("capture thread exited unexpectedly".into())
            }
        }
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Human-readable description of what a "source" is on this OS.
pub const SOURCE_KIND: &str = if cfg!(target_os = "linux") { "audio stream" } else { "window" };
