//! Background listening + recognition loop.

use crate::capture::{Capture, Source};
use crate::resample::Resampler;
use crate::shazam::{self, Track};
use crate::signature::{SignatureGenerator, SAMPLE_RATE};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_SECS: usize = 12;
const MAX_SAMPLES: usize = MAX_SECS * SAMPLE_RATE as usize;
/// Attempt schedule (seconds of buffered audio) for the first tries on a song.
const FIRST_TRIES: [usize; 3] = [4, 7, 10];

#[derive(Debug)]
pub enum Event {
    Status(String),
    Matched(Track),
    NoMatch(u32),
    Error(String),
    /// Listening ended (single-shot match or stream died).
    Finished(Option<String>),
}

pub struct Engine {
    stop: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    buffered: Arc<AtomicU32>,
    _capture: Capture,
    worker: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn start(
        src: &Source,
        continuous: bool,
        tx: Sender<Event>,
        repaint: impl Fn() + Send + Sync + Clone + 'static,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let buffered = Arc::new(AtomicU32::new(0));
        let buf: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_SAMPLES + 4096)));

        let cb = {
            let buf = buf.clone();
            let level = level.clone();
            let buffered = buffered.clone();
            let mut rs: Option<Resampler> = None;
            let mut out = Vec::with_capacity(8192);
            Box::new(move |samples: &[f32], ch: usize, rate: u32| {
                if rs.as_ref().map(|r| r.in_rate()) != Some(rate) {
                    rs = Some(Resampler::new(rate));
                }
                let peak = samples.iter().fold(0f32, |m, &x| m.max(x.abs()));
                let prev = f32::from_bits(level.load(Ordering::Relaxed));
                // Fast attack, slow release, for a pleasant meter.
                let v = if peak > prev { peak } else { prev * 0.85 + peak * 0.15 };
                level.store(v.to_bits(), Ordering::Relaxed);

                out.clear();
                rs.as_mut().unwrap().push(samples, ch, &mut out);
                let mut b = buf.lock().unwrap();
                b.extend(out.iter().copied());
                let excess = b.len().saturating_sub(MAX_SAMPLES);
                b.drain(..excess);
                buffered.store(b.len() as u32, Ordering::Relaxed);
            }) as crate::capture::AudioCallback
        };

        let on_end = {
            let tx = tx.clone();
            let repaint = repaint.clone();
            Box::new(move |e: String| {
                let _ = tx.send(Event::Finished(Some(e)));
                repaint();
            })
        };
        let capture = Capture::start(src, cb, on_end)?;

        let worker = {
            let stop = stop.clone();
            let buffered = buffered.clone();
            std::thread::Builder::new()
                .name("recognise".into())
                .spawn(move || worker(buf, buffered, stop, continuous, tx, repaint))
                .map_err(|e| e.to_string())?
        };

        Ok(Self { stop, level, buffered, _capture: capture, worker: Some(worker) })
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn buffered_secs(&self) -> f32 {
        self.buffered.load(Ordering::Relaxed) as f32 / SAMPLE_RATE as f32
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Worker may be mid-request; don't block the UI waiting for it.
        drop(self.worker.take());
    }
}

fn rms(s: &[i16]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    let sum: f64 = s.iter().map(|&x| (x as f64) * (x as f64)).sum();
    ((sum / s.len() as f64).sqrt() / 32768.0) as f32
}

fn worker(
    buf: Arc<Mutex<VecDeque<i16>>>,
    buffered: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    continuous: bool,
    tx: Sender<Event>,
    repaint: impl Fn(),
) {
    let send = |e: Event| {
        let _ = tx.send(e);
        repaint();
    };
    let mut last_key: Option<String> = None;
    let mut tries = 0usize; // attempts on the current song
    let mut misses = 0u32;
    let mut next_at = Instant::now();
    let mut waiting_msg = false;

    send(Event::Status("Listening…".into()));
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
        if Instant::now() < next_at {
            continue;
        }
        let need = FIRST_TRIES.get(tries).copied().unwrap_or(MAX_SECS) * SAMPLE_RATE as usize;
        if (buffered.load(Ordering::Relaxed) as usize) < need {
            continue;
        }
        let pcm: Vec<i16> = buf.lock().unwrap().iter().copied().collect();
        if rms(&pcm[pcm.len().saturating_sub(4 * SAMPLE_RATE as usize)..]) < 0.002 {
            if !waiting_msg {
                send(Event::Status("Waiting for sound…".into()));
                waiting_msg = true;
            }
            next_at = Instant::now() + Duration::from_secs(1);
            continue;
        }
        waiting_msg = false;

        send(Event::Status("Identifying…".into()));
        let sig = SignatureGenerator::generate(&pcm);
        let result = shazam::recognise(&sig);
        // The user may have pressed stop while the request was in flight.
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match result {
            Ok(Some(track)) => {
                misses = 0;
                tries = 0;
                let new = last_key.as_deref() != Some(track.key.as_str());
                last_key = Some(track.key.clone());
                if new {
                    send(Event::Matched(track));
                }
                if !continuous {
                    send(Event::Finished(None));
                    return;
                }
                send(Event::Status("Listening for the next song…".into()));
                // Re-check periodically for a track change, with fresh audio.
                buf.lock().unwrap().clear();
                buffered.store(0, Ordering::Relaxed);
                next_at = Instant::now() + Duration::from_secs(15);
            }
            Ok(None) => {
                misses += 1;
                tries += 1;
                send(Event::NoMatch(misses));
                next_at = Instant::now() + Duration::from_secs(3);
            }
            Err(e) => {
                send(Event::Error(e));
                next_at = Instant::now() + Duration::from_secs(5);
            }
        }
    }
}
