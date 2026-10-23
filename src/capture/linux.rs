//! Linux: PulseAudio / PipeWire (via pipewire-pulse).
//!
//! Sources are playback streams ("sink inputs"); each one is recorded in
//! isolation with `parec --monitor-stream`. This works on both X11 and
//! Wayland, where window enumeration is not generally possible.

use super::{AudioCallback, Source};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const RATE: u32 = 16_000;

fn missing_tools(e: std::io::Error) -> String {
    format!(
        "{e}. Install the PulseAudio utilities (package `pulseaudio-utils` or `libpulse`); \
         they also work with PipeWire through pipewire-pulse."
    )
}

pub fn list_sources() -> Result<Vec<Source>, String> {
    let out = Command::new("pactl")
        .args(["list", "sink-inputs"])
        .env("LC_ALL", "C")
        .output()
        .map_err(missing_tools)?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(parse_sink_inputs(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_sink_inputs(text: &str) -> Vec<Source> {
    let me = std::process::id();
    let mut out = Vec::new();
    let mut cur: Option<(u64, bool, Vec<(String, String)>)> = None;

    let flush = |cur: &mut Option<(u64, bool, Vec<(String, String)>)>, out: &mut Vec<Source>| {
        let Some((idx, corked, props)) = cur.take() else { return };
        let get = |k: &str| props.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone());
        let pid: u32 = get("application.process.id").and_then(|p| p.parse().ok()).unwrap_or(0);
        if pid == me {
            return;
        }
        let app = get("application.name")
            .or_else(|| get("application.process.binary"))
            .or_else(|| get("node.name"))
            .unwrap_or_else(|| format!("Stream {idx}"));
        let media = get("media.name").unwrap_or_default();
        let generic = ["playback", "audio stream", "audiostream", "output", ""];
        let title = if generic.contains(&media.to_lowercase().as_str()) { app.clone() } else { media };
        out.push(Source { id: idx, pid, title, app, audible: !corked });
    };

    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("Sink Input #") {
            flush(&mut cur, &mut out);
            if let Ok(idx) = rest.trim().parse() {
                cur = Some((idx, false, Vec::new()));
            }
        } else if let Some((_, corked, props)) = cur.as_mut() {
            if let Some(v) = l.strip_prefix("Corked:") {
                *corked = v.trim() == "yes";
            } else if let Some((k, v)) = l.split_once(" = ") {
                props.push((k.trim().to_string(), v.trim().trim_matches('"').to_string()));
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

pub fn run(
    src: &Source,
    cb: &mut AudioCallback,
    stop: &AtomicBool,
    ready: impl FnOnce(Result<(), String>),
) -> Result<(), String> {
    let mut child = Command::new("parec")
        .arg(format!("--monitor-stream={}", src.id))
        .args(["--format=s16le", "--channels=1", "--latency-msec=60"])
        .arg(format!("--rate={RATE}"))
        .args(["--client-name=Music Identifier", "--stream-name=identify"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(missing_tools)?;
    let mut stdout = child.stdout.take().unwrap();

    // Reads happen on their own thread so a silent (corked) stream can't
    // block shutdown.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut chunk = vec![0u8; RATE as usize / 10 * 2];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Give parec a moment to fail fast on a bad stream index.
    std::thread::sleep(Duration::from_millis(150));
    if let Ok(Some(status)) = child.try_wait() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        let msg = format!("parec exited ({status}): {}", err.trim());
        ready(Err(msg.clone()));
        return Err(msg);
    }
    ready(Ok(()));

    let mut carry: Option<u8> = None;
    let mut samples = Vec::with_capacity(RATE as usize);
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => {
                samples.clear();
                let mut it = bytes.iter().copied();
                if let Some(lo) = carry.take() {
                    if let Some(hi) = it.next() {
                        samples.push(i16::from_le_bytes([lo, hi]) as f32 / 32768.0);
                    }
                }
                let rest: Vec<u8> = it.collect();
                for pair in rest.chunks(2) {
                    if pair.len() == 2 {
                        samples.push(i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0);
                    } else {
                        carry = Some(pair[0]);
                    }
                }
                cb(&samples, 1, RATE);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err("the audio stream ended (playback stopped or the app closed)".to_string());
            }
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_pactl() {
        let t = "Sink Input #57\n\tDriver: PipeWire\n\tCorked: no\n\tProperties:\n\t\tmedia.name = \"Lo-fi beats - YouTube\"\n\t\tapplication.name = \"Firefox\"\n\t\tapplication.process.id = \"4242\"\n\nSink Input #58\n\tCorked: yes\n\tProperties:\n\t\tmedia.name = \"Playback\"\n\t\tapplication.name = \"Spotify\"\n";
        let v = super::parse_sink_inputs(t);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, 57);
        assert_eq!(v[0].title, "Lo-fi beats - YouTube");
        assert!(v[0].audible);
        assert_eq!(v[1].title, "Spotify");
        assert!(!v[1].audible);
    }
}
