//! Streaming down-mix + windowed-sinc resampler to 16 kHz mono i16.

use crate::signature::SAMPLE_RATE;

const HALF_TAPS: i64 = 16;

pub struct Resampler {
    in_rate: u32,
    step: f64,   // input samples per output sample
    pos: f64,    // fractional read position into `hist`, relative to its start
    hist: Vec<f32>,
    cutoff: f64, // normalised to input rate
}

impl Resampler {
    pub fn new(in_rate: u32) -> Self {
        let step = in_rate as f64 / SAMPLE_RATE as f64;
        Self {
            in_rate,
            step,
            pos: HALF_TAPS as f64,
            hist: vec![0.0; HALF_TAPS as usize],
            // Leave a little headroom below Nyquist of the output rate.
            cutoff: 0.45 / step.max(1.0),
        }
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    /// Push interleaved f32 frames; resampled output is appended to `out`.
    pub fn push(&mut self, interleaved: &[f32], channels: usize, out: &mut Vec<i16>) {
        let ch = channels.max(1);
        self.hist.extend(
            interleaved
                .chunks_exact(ch)
                .map(|f| f.iter().sum::<f32>() / ch as f32),
        );

        if self.in_rate == SAMPLE_RATE {
            let n = self.hist.len() - HALF_TAPS as usize;
            out.extend(self.hist[..n].iter().map(|&x| to_i16(x)));
            self.hist.drain(..n);
            return;
        }

        let c = self.cutoff;
        while self.pos + (HALF_TAPS as f64) < self.hist.len() as f64 {
            let center = self.pos.floor() as i64;
            let frac = self.pos - center as f64;
            let mut acc = 0.0f64;
            let mut norm = 0.0f64;
            for k in -HALF_TAPS + 1..=HALF_TAPS {
                let x = k as f64 - frac;
                let sinc = if x.abs() < 1e-9 {
                    2.0 * c
                } else {
                    (2.0 * std::f64::consts::PI * c * x).sin() / (std::f64::consts::PI * x)
                };
                // Blackman window
                let w = 0.42
                    + 0.5 * (std::f64::consts::PI * x / HALF_TAPS as f64).cos()
                    + 0.08 * (2.0 * std::f64::consts::PI * x / HALF_TAPS as f64).cos();
                let h = sinc * w;
                acc += self.hist[(center + k) as usize] as f64 * h;
                norm += h;
            }
            out.push(to_i16((acc / norm) as f32));
            self.pos += self.step;
        }

        // Drop consumed history, keeping enough for the next filter window.
        let keep_from = (self.pos.floor() as i64 - HALF_TAPS).max(0) as usize;
        if keep_from > 0 {
            self.hist.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }
}

#[inline]
fn to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32767.0) as i16
}
