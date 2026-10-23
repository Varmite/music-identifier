//! Shazam-compatible audio fingerprinting.
//!
//! This is a Rust port of the algorithm reverse-engineered by the SongRec
//! project (GPL-3.0). Input is 16 kHz mono i16 PCM; output is the binary
//! signature the Shazam service expects.

use base64::Engine;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 16_000;

const FFT_SIZE: usize = 2048;
const BINS: usize = FFT_SIZE / 2 + 1; // 1025
const HISTORY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Band {
    B250_520 = 0,
    B520_1450 = 1,
    B1450_3500 = 2,
    B3500_5500 = 3,
}

#[derive(Clone, Copy, Debug)]
struct Peak {
    fft_pass_number: u32,
    magnitude: u16,
    corrected_bin: u16,
}

pub struct Signature {
    number_samples: u32,
    bands: [Vec<Peak>; 4],
}

impl Signature {
    pub fn duration_ms(&self) -> u32 {
        (self.number_samples as u64 * 1000 / SAMPLE_RATE as u64) as u32
    }

    pub fn peak_count(&self) -> usize {
        self.bands.iter().map(Vec::len).sum()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(48 + 8 + self.peak_count() * 5 + 64);
        let push32 = |o: &mut Vec<u8>, v: u32| o.extend_from_slice(&v.to_le_bytes());

        // --- 48-byte header (crc and size patched below) ---
        push32(&mut out, 0xcafe_2580); // magic1
        push32(&mut out, 0); // crc32
        push32(&mut out, 0); // size minus header
        push32(&mut out, 0x9411_9c00); // magic2
        for _ in 0..3 {
            push32(&mut out, 0);
        }
        push32(&mut out, 3 << 27); // sample-rate id 3 == 16 kHz
        for _ in 0..2 {
            push32(&mut out, 0);
        }
        push32(
            &mut out,
            self.number_samples + (SAMPLE_RATE as f32 * 0.24) as u32,
        );
        push32(&mut out, (15 << 19) + 0x40000);

        push32(&mut out, 0x4000_0000);
        push32(&mut out, 0); // size minus header (again)

        for (band, peaks) in self.bands.iter().enumerate() {
            if peaks.is_empty() {
                continue;
            }
            let mut buf = Vec::with_capacity(peaks.len() * 5);
            let mut pass = 0u32;
            for p in peaks {
                if p.fft_pass_number - pass >= 255 {
                    buf.push(0xff);
                    buf.extend_from_slice(&p.fft_pass_number.to_le_bytes());
                    pass = p.fft_pass_number;
                }
                buf.push((p.fft_pass_number - pass) as u8);
                buf.extend_from_slice(&p.magnitude.to_le_bytes());
                buf.extend_from_slice(&p.corrected_bin.to_le_bytes());
                pass = p.fft_pass_number;
            }
            push32(&mut out, 0x6003_0040 + band as u32);
            push32(&mut out, buf.len() as u32);
            let pad = (4 - buf.len() % 4) % 4;
            out.extend_from_slice(&buf);
            out.extend(std::iter::repeat_n(0, pad));
        }

        let size = (out.len() - 48) as u32;
        out[8..12].copy_from_slice(&size.to_le_bytes());
        out[52..56].copy_from_slice(&size.to_le_bytes());
        let crc = crc32fast::hash(&out[8..]);
        out[4..8].copy_from_slice(&crc.to_le_bytes());
        out
    }

    pub fn to_data_uri(&self) -> String {
        format!(
            "data:audio/vnd.shazam.sig;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(self.encode())
        )
    }
}

pub struct SignatureGenerator {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    samples: Vec<i16>, // ring of FFT_SIZE
    samples_pos: usize,
    fft_in: Vec<f32>,
    fft_out: Vec<realfft::num_complex::Complex<f32>>,
    scratch: Vec<realfft::num_complex::Complex<f32>>,
    ffts: Vec<Vec<f32>>, // ring of HISTORY
    ffts_pos: usize,
    spread: Vec<Vec<f32>>, // ring of HISTORY
    spread_pos: usize,
    spread_done: u32,
    bands: [Vec<Peak>; 4],
}

impl SignatureGenerator {
    pub fn new() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        // Hanning window of length 2050 with the two zero endpoints dropped.
        let n = 2050f32;
        let window = (1..=FFT_SIZE)
            .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (n - 1.0)).cos()))
            .collect();
        let fft_in = fft.make_input_vec();
        let fft_out = fft.make_output_vec();
        let scratch = fft.make_scratch_vec();
        Self {
            fft,
            window,
            samples: vec![0; FFT_SIZE],
            samples_pos: 0,
            fft_in,
            fft_out,
            scratch,
            ffts: vec![vec![0.0; BINS]; HISTORY],
            ffts_pos: 0,
            spread: vec![vec![0.0; BINS]; HISTORY],
            spread_pos: 0,
            spread_done: 0,
            bands: Default::default(),
        }
    }

    /// Fingerprint a 16 kHz mono buffer (ideally 8–12 s long).
    pub fn generate(pcm: &[i16]) -> Signature {
        let mut g = Self::new();
        for chunk in pcm.chunks_exact(128) {
            g.do_fft(chunk);
            g.do_peak_spreading();
            g.spread_done += 1;
            if g.spread_done >= 46 {
                g.do_peak_recognition();
            }
        }
        Signature {
            number_samples: pcm.len() as u32,
            bands: g.bands,
        }
    }

    #[inline]
    fn idx(pos: usize, offset: i32) -> usize {
        ((pos as i32 + offset) & (HISTORY as i32 - 1)) as usize
    }

    fn do_fft(&mut self, chunk: &[i16]) {
        self.samples[self.samples_pos..self.samples_pos + 128].copy_from_slice(chunk);
        self.samples_pos = (self.samples_pos + 128) & (FFT_SIZE - 1);

        for (i, w) in self.window.iter().enumerate() {
            self.fft_in[i] = self.samples[(i + self.samples_pos) & (FFT_SIZE - 1)] as f32 * w;
        }
        self.fft
            .process_with_scratch(&mut self.fft_in, &mut self.fft_out, &mut self.scratch)
            .expect("fft size mismatch");

        let dst = &mut self.ffts[self.ffts_pos];
        for (d, c) in dst.iter_mut().zip(&self.fft_out) {
            *d = ((c.re * c.re + c.im * c.im) / (1 << 17) as f32).max(1e-10);
        }
        self.ffts_pos = (self.ffts_pos + 1) & (HISTORY - 1);
    }

    fn do_peak_spreading(&mut self) {
        let mut s = self.ffts[Self::idx(self.ffts_pos, -1)].clone();
        for p in 0..=1022 {
            s[p] = s[p].max(s[p + 1]).max(s[p + 2]);
        }
        for former in [1, 3, 6] {
            let f = &mut self.spread[Self::idx(self.spread_pos, -former)];
            for p in 0..BINS {
                f[p] = f[p].max(s[p]);
            }
        }
        self.spread[self.spread_pos] = s;
        self.spread_pos = (self.spread_pos + 1) & (HISTORY - 1);
    }

    fn do_peak_recognition(&mut self) {
        let fft46 = &self.ffts[Self::idx(self.ffts_pos, -46)];
        let spread49 = &self.spread[Self::idx(self.spread_pos, -49)];
        let mag = |v: f32| (v.max(1.0 / 64.0)).ln() * 1477.3 + 6144.0;

        for bin in 10..1015usize {
            let v = fft46[bin];
            if !(v >= 1.0 / 64.0 && v >= spread49[bin - 1]) {
                continue;
            }
            let mut max_n = 0f32;
            for off in [-10i32, -7, -4, -3, 1, 2, 5, 8] {
                max_n = max_n.max(spread49[(bin as i32 + off) as usize]);
            }
            if v <= max_n {
                continue;
            }
            let mut max_o = max_n;
            for off in [-53, -45, 165, 172, 179, 186, 193, 200, 214, 221, 228, 235, 242, 249] {
                max_o = max_o.max(self.spread[Self::idx(self.spread_pos, off)][bin - 1]);
            }
            if v <= max_o {
                continue;
            }

            let m = mag(v);
            let before = mag(fft46[bin - 1]);
            let after = mag(fft46[bin + 1]);
            let var1 = m * 2.0 - before - after;
            let var2 = (after - before) * 32.0 / var1;
            let corrected = (bin as f32 * 64.0 + var2) as u16;
            let hz = corrected as f32 * (16000.0 / 2.0 / 1024.0 / 64.0);
            let band = match hz as i32 {
                250..=519 => Band::B250_520,
                520..=1449 => Band::B520_1450,
                1450..=3499 => Band::B1450_3500,
                3500..=5500 => Band::B3500_5500,
                _ => continue,
            };
            self.bands[band as usize].push(Peak {
                fft_pass_number: self.spread_done - 46,
                magnitude: m as u16,
                corrected_bin: corrected,
            });
        }
    }
}
