//! macOS: ScreenCaptureKit (macOS 13+). Captures the audio of the app that
//! owns the selected window. Requires the Screen Recording permission.

use super::{AudioCallback, Source};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::time::Duration;

const PERMISSION_HINT: &str = "Grant Screen Recording permission to this app in \
    System Settings → Privacy & Security → Screen & System Audio Recording, then restart it.";

struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

fn ns_err(e: *mut NSError) -> String {
    match unsafe { e.as_ref() } {
        Some(e) => e.localizedDescription().to_string(),
        None => "unknown error".into(),
    }
}

fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = mpsc::channel::<SendBox<Result<Retained<SCShareableContent>, String>>>();
    let block = RcBlock::new(move |c: *mut SCShareableContent, e: *mut NSError| {
        let r = match unsafe { Retained::retain(c) } {
            Some(c) => Ok(c),
            None => Err(format!("{}. {PERMISSION_HINT}", ns_err(e))),
        };
        let _ = tx.send(SendBox(r));
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|_| "timed out listing windows".to_string())?
        .0
}

pub fn list_sources() -> Result<Vec<Source>, String> {
    let content = shareable_content()?;
    let me = std::process::id() as i32;
    let mut out = Vec::new();
    unsafe {
        for w in content.windows().iter() {
            if !w.isOnScreen() || w.windowLayer() != 0 {
                continue;
            }
            let Some(app) = w.owningApplication() else { continue };
            if app.processID() == me {
                continue;
            }
            let app_name = app.applicationName().to_string();
            let title = w.title().map(|t| t.to_string()).unwrap_or_default();
            if title.is_empty() && app_name.is_empty() {
                continue;
            }
            out.push(Source {
                id: w.windowID() as u64,
                pid: app.processID() as u32,
                title: if title.is_empty() { app_name.clone() } else { title },
                app: app_name,
                audible: false,
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- stream output

enum Msg {
    Audio(Vec<f32>, usize, u32),
    Stopped(String),
}

struct Ivars {
    tx: Mutex<Sender<Msg>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and we don't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "MusicIdentifierStreamOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output(&self, _s: &SCStream, sb: &CMSampleBuffer, ty: SCStreamOutputType) {
            if ty != SCStreamOutputType::Audio {
                return;
            }
            if let Some(m) = unsafe { extract_audio(sb) } {
                let _ = self.ivars().tx.lock().unwrap().send(m);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_did_stop(&self, _s: &SCStream, err: &NSError) {
            let msg = err.localizedDescription().to_string();
            let _ = self.ivars().tx.lock().unwrap().send(Msg::Stopped(msg));
        }
    }
);

impl Output {
    fn new(tx: Sender<Msg>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { tx: Mutex::new(tx) });
        unsafe { msg_send![super(this), init] }
    }
}

/// Copy a CMSampleBuffer of linear PCM into interleaved f32.
unsafe fn extract_audio(sb: &CMSampleBuffer) -> Option<Msg> {
    let fd = unsafe { sb.format_description() }?;
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&fd).as_ref() }?;
    let rate = asbd.mSampleRate as u32;
    let channels = asbd.mChannelsPerFrame.max(1) as usize;
    let is_float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
    let non_interleaved = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
    if !is_float || asbd.mBitsPerChannel != 32 {
        return None; // ScreenCaptureKit delivers f32; ignore anything else.
    }

    // Room for up to 8 non-interleaved buffers.
    let size = size_of::<AudioBufferList>() + 7 * size_of::<AudioBuffer>();
    let mut storage = vec![0u64; size.div_ceil(8)];
    let abl = storage.as_mut_ptr() as *mut AudioBufferList;
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    let status = unsafe {
        sb.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            abl,
            size,
            None,
            None,
            1, // kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment
            &mut block,
        )
    };
    // Release the retained block buffer when we're done.
    let _block = NonNull::new(block).map(|b| unsafe { CFRetained::from_raw(b) });
    if status != 0 {
        return None;
    }

    let n_bufs = unsafe { (*abl).mNumberBuffers } as usize;
    let bufs = unsafe { std::slice::from_raw_parts((*abl).mBuffers.as_ptr(), n_bufs) };
    let mut out = Vec::new();
    if non_interleaved && n_bufs > 1 {
        let planes: Vec<&[f32]> = bufs
            .iter()
            .map(|b| unsafe {
                std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4)
            })
            .collect();
        let frames = planes.iter().map(|p| p.len()).min().unwrap_or(0);
        out.reserve(frames * planes.len());
        for i in 0..frames {
            for p in &planes {
                out.push(p[i]);
            }
        }
        Some(Msg::Audio(out, planes.len(), rate))
    } else {
        let b = bufs.first()?;
        let s = unsafe { std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4) };
        out.extend_from_slice(s);
        let ch = if non_interleaved { 1 } else { channels };
        Some(Msg::Audio(out, ch, rate))
    }
}

fn wait_completion(start: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let block = RcBlock::new(move |e: *mut NSError| {
        let _ = tx.send(if e.is_null() { Ok(()) } else { Err(ns_err(e)) });
    });
    start(&block);
    rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "timed out".to_string())?
}

pub fn run(
    src: &Source,
    cb: &mut AudioCallback,
    stop: &AtomicBool,
    ready: impl FnOnce(Result<(), String>),
) -> Result<(), String> {
    let content = shareable_content()?;
    let (tx, rx) = mpsc::channel::<Msg>();
    let output = Output::new(tx);

    let stream = unsafe {
        let window = content
            .windows()
            .iter()
            .find(|w| w.windowID() as u64 == src.id)
            .ok_or("the window has closed")?;
        let app = window.owningApplication().ok_or("window has no owning app")?;
        let display = content.displays().firstObject().ok_or("no display found")?;
        let filter = SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::from_retained_slice(&[app]),
            &NSArray::new(),
        );

        let cfg = SCStreamConfiguration::new();
        cfg.setCapturesAudio(true);
        cfg.setSampleRate(48_000);
        cfg.setChannelCount(2);
        cfg.setExcludesCurrentProcessAudio(true);
        // We only want audio; keep the video side as cheap as possible.
        cfg.setWidth(2);
        cfg.setHeight(2);
        cfg.setMinimumFrameInterval(CMTime::new(1, 1));
        cfg.setQueueDepth(3);

        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &cfg,
            Some(ProtocolObject::from_ref(&*output)),
        );
        let queue = DispatchQueue::new("music-identifier.audio", None);
        let out_proto = ProtocolObject::from_ref(&*output);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(out_proto, SCStreamOutputType::Audio, Some(&queue))
            .map_err(|e| e.localizedDescription().to_string())?;
        // A screen output too, otherwise ScreenCaptureKit logs every dropped frame.
        let _ = stream.addStreamOutput_type_sampleHandlerQueue_error(
            out_proto,
            SCStreamOutputType::Screen,
            Some(&queue),
        );
        stream
    };

    if let Err(e) = wait_completion(|b| unsafe { stream.startCaptureWithCompletionHandler(Some(b)) }) {
        let msg = format!("{e}. {PERMISSION_HINT}");
        ready(Err(msg.clone()));
        return Err(msg);
    }
    ready(Ok(()));

    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Msg::Audio(s, ch, rate)) => cb(&s, ch, rate),
            Ok(Msg::Stopped(e)) => break Err(format!("capture stopped: {e}")),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
        }
    };
    let _ = wait_completion(|b| unsafe { stream.stopCaptureWithCompletionHandler(Some(b)) });
    result
}
