//! Acoustic simulator: turns a magnetic-pickup electric guitar DI into
//! something that sounds like an acoustic.
//!
//! Chain: pickup-correction EQ -> body impulse response (convolution) -> small room.
//! The body IR is either synthesised (resonant modes of a guitar body) or a real
//! acoustic-sim IR loaded from a WAV file.

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};
use std::f32::consts::PI;
use std::sync::Arc;

/// Convolution block size. Adds this many samples of latency (~1.3 ms at 48 kHz).
pub const CONV_BLOCK: usize = 64;
/// Longest IR we accept, in seconds.
pub const MAX_IR_SECONDS: f32 = 1.0;

// ---------------------------------------------------------------------------
// Biquad (RBJ cookbook)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::identity()
    }
}

impl Biquad {
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn norm(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn w0(sr: f32, f: f32) -> (f32, f32) {
        let w = 2.0 * PI * (f / sr).min(0.49);
        (w.cos(), w.sin())
    }

    pub fn highpass(sr: f32, f: f32, q: f32) -> Self {
        let (c, s) = Self::w0(sr, f);
        let alpha = s / (2.0 * q);
        Self::norm(
            (1.0 + c) / 2.0,
            -(1.0 + c),
            (1.0 + c) / 2.0,
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        )
    }

    pub fn peaking(sr: f32, f: f32, q: f32, db: f32) -> Self {
        let a = 10f32.powf(db / 40.0);
        let (c, s) = Self::w0(sr, f);
        let alpha = s / (2.0 * q);
        Self::norm(
            1.0 + alpha * a,
            -2.0 * c,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * c,
            1.0 - alpha / a,
        )
    }

    pub fn high_shelf(sr: f32, f: f32, db: f32) -> Self {
        let a = 10f32.powf(db / 40.0);
        let (c, s) = Self::w0(sr, f);
        let alpha = s / 2.0 * 2f32.sqrt();
        let sa = 2.0 * a.sqrt() * alpha;
        Self::norm(
            a * ((a + 1.0) + (a - 1.0) * c + sa),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
            a * ((a + 1.0) + (a - 1.0) * c - sa),
            (a + 1.0) - (a - 1.0) * c + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - sa,
        )
    }

    pub fn lowpass(sr: f32, f: f32, q: f32) -> Self {
        let (c, s) = Self::w0(sr, f);
        let alpha = s / (2.0 * q);
        Self::norm(
            (1.0 - c) / 2.0,
            1.0 - c,
            (1.0 - c) / 2.0,
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        )
    }

    pub fn low_shelf(sr: f32, f: f32, db: f32) -> Self {
        let a = 10f32.powf(db / 40.0);
        let (c, s) = Self::w0(sr, f);
        let alpha = s / 2.0 * 2f32.sqrt();
        let sa = 2.0 * a.sqrt() * alpha;
        Self::norm(
            a * ((a + 1.0) - (a - 1.0) * c + sa),
            2.0 * a * ((a - 1.0) - (a + 1.0) * c),
            a * ((a + 1.0) - (a - 1.0) * c - sa),
            (a + 1.0) + (a - 1.0) * c + sa,
            -2.0 * ((a - 1.0) + (a + 1.0) * c),
            (a + 1.0) + (a - 1.0) * c - sa,
        )
    }

    /// Take new coefficients but keep the filter state (no clicks on knob moves).
    pub fn retune(&mut self, other: Biquad) {
        let (z1, z2) = (self.z1, self.z2);
        *self = other;
        self.z1 = z1;
        self.z2 = z2;
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

// ---------------------------------------------------------------------------
// Uniformly partitioned FFT convolution (overlap-save)
// ---------------------------------------------------------------------------

/// Pre-computed spectra of an impulse response, shared between convolver instances.
pub struct IrSpectrum {
    pub name: String,
    parts: Vec<Vec<Complex<f32>>>,
}

impl IrSpectrum {
    pub fn new(name: impl Into<String>, ir: &[f32]) -> Self {
        let n = 2 * CONV_BLOCK;
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(n);
        let mut parts = Vec::new();
        for chunk in ir.chunks(CONV_BLOCK) {
            let mut buf = vec![0.0; n];
            buf[..chunk.len()].copy_from_slice(chunk);
            let mut spec = fft.make_output_vec();
            fft.process(&mut buf, &mut spec).expect("fft sizes match");
            parts.push(spec);
        }
        if parts.is_empty() {
            parts.push(fft.make_output_vec());
        }
        Self {
            name: name.into(),
            parts,
        }
    }
}

pub struct Convolver {
    ir: Arc<IrSpectrum>,
    fft: Arc<dyn RealToComplex<f32>>,
    ifft: Arc<dyn ComplexToReal<f32>>,
    inbuf: Vec<f32>,
    outbuf: Vec<f32>,
    time: Vec<f32>,
    fdl: Vec<Vec<Complex<f32>>>,
    acc: Vec<Complex<f32>>,
    scratch_f: Vec<Complex<f32>>,
    scratch_i: Vec<Complex<f32>>,
    head: usize,
    pos: usize,
}

impl Convolver {
    pub fn new(ir: Arc<IrSpectrum>) -> Self {
        let n = 2 * CONV_BLOCK;
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n);
        let ifft = planner.plan_fft_inverse(n);
        let fdl = vec![fft.make_output_vec(); ir.parts.len()];
        Self {
            acc: fft.make_output_vec(),
            scratch_f: fft.make_scratch_vec(),
            scratch_i: ifft.make_scratch_vec(),
            inbuf: vec![0.0; n],
            outbuf: vec![0.0; CONV_BLOCK],
            time: vec![0.0; n],
            fdl,
            head: 0,
            pos: 0,
            fft,
            ifft,
            ir,
        }
    }

    pub fn reset(&mut self) {
        self.inbuf.fill(0.0);
        self.outbuf.fill(0.0);
        for s in &mut self.fdl {
            s.fill(Complex::default());
        }
        self.head = 0;
        self.pos = 0;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.outbuf[self.pos];
        self.inbuf[CONV_BLOCK + self.pos] = x;
        self.pos += 1;
        if self.pos == CONV_BLOCK {
            self.pos = 0;
            self.run_block();
        }
        y
    }

    fn run_block(&mut self) {
        let parts = self.fdl.len();
        self.time.copy_from_slice(&self.inbuf);
        let _ = self.fft.process_with_scratch(
            &mut self.time,
            &mut self.fdl[self.head],
            &mut self.scratch_f,
        );

        self.acc.fill(Complex::default());
        for k in 0..parts {
            let x = &self.fdl[(self.head + parts - k) % parts];
            let h = &self.ir.parts[k];
            for ((a, x), h) in self.acc.iter_mut().zip(x).zip(h) {
                *a += x * h;
            }
        }
        let last = self.acc.len() - 1;
        self.acc[0].im = 0.0;
        self.acc[last].im = 0.0;
        let _ = self
            .ifft
            .process_with_scratch(&mut self.acc, &mut self.time, &mut self.scratch_i);

        let scale = 1.0 / (2 * CONV_BLOCK) as f32;
        for (o, t) in self.outbuf.iter_mut().zip(&self.time[CONV_BLOCK..]) {
            *o = t * scale;
        }
        self.inbuf.copy_within(CONV_BLOCK.., 0);
        self.head = (self.head + 1) % parts;
    }
}

// ---------------------------------------------------------------------------
// Small room reverb (Freeverb-style, mono)
// ---------------------------------------------------------------------------

struct Comb {
    buf: Vec<f32>,
    idx: usize,
    store: f32,
}

struct Allpass {
    buf: Vec<f32>,
    idx: usize,
}

pub struct Room {
    combs: Vec<Comb>,
    aps: Vec<Allpass>,
}

impl Room {
    pub fn new(sr: f32) -> Self {
        let k = sr / 44100.0;
        let len = |n: usize| ((n as f32 * k) as usize).max(1);
        Self {
            combs: [1116, 1188, 1277, 1356, 1422, 1491]
                .iter()
                .map(|&n| Comb {
                    buf: vec![0.0; len(n)],
                    idx: 0,
                    store: 0.0,
                })
                .collect(),
            aps: [556, 441, 341, 225]
                .iter()
                .map(|&n| Allpass {
                    buf: vec![0.0; len(n)],
                    idx: 0,
                })
                .collect(),
        }
    }

    pub fn reset(&mut self) {
        for c in &mut self.combs {
            c.buf.fill(0.0);
            c.store = 0.0;
        }
        for a in &mut self.aps {
            a.buf.fill(0.0);
        }
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        const FEEDBACK: f32 = 0.78; // small room
        const DAMP: f32 = 0.35;
        let input = x * 0.015;
        let mut out = 0.0;
        for c in &mut self.combs {
            let y = c.buf[c.idx];
            c.store = y * (1.0 - DAMP) + c.store * DAMP;
            c.buf[c.idx] = input + c.store * FEEDBACK;
            c.idx = (c.idx + 1) % c.buf.len();
            out += y;
        }
        for a in &mut self.aps {
            let b = a.buf[a.idx];
            a.buf[a.idx] = out + b * 0.5;
            a.idx = (a.idx + 1) % a.buf.len();
            out = b - out;
        }
        out * 3.0
    }
}

// ---------------------------------------------------------------------------
// The acoustic simulator
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SimParams {
    /// Master switch for monitoring and for tracks marked "acoustic".
    pub enabled: bool,
    /// Tame the magnetic-pickup character (low end mud, mid honk, pickup resonance).
    pub pickup_eq: bool,
    /// Amount of body resonance (0 = dry, 1 = full IR).
    pub body: f32,
    /// High shelf, dB. Magnetic pickups roll off the top that an acoustic has.
    pub brightness_db: f32,
    /// Low "box" resonance, dB.
    pub warmth_db: f32,
    /// Small room reverb amount.
    pub room: f32,
    /// Output level, dB.
    pub level_db: f32,
    /// Path of a loaded IR wav, or None for the built-in body.
    pub ir_path: Option<String>,
}

impl Default for SimParams {
    fn default() -> Self {
        Self {
            enabled: true,
            pickup_eq: true,
            body: 0.85,
            brightness_db: 6.0,
            warmth_db: 3.0,
            room: 0.25,
            level_db: 0.0,
            ir_path: None,
        }
    }
}

pub struct AcousticSim {
    sr: f32,
    p: SimParams,
    level: f32,
    eq_fixed: [Biquad; 3],
    warmth: Biquad,
    bright: Biquad,
    conv: Convolver,
    dry_delay: [f32; CONV_BLOCK],
    dry_idx: usize,
    room: Room,
}

impl AcousticSim {
    pub fn new(sr: f32, p: &SimParams, ir: Arc<IrSpectrum>) -> Self {
        let mut s = Self {
            sr,
            p: p.clone(),
            level: 1.0,
            eq_fixed: [Biquad::identity(); 3],
            warmth: Biquad::identity(),
            bright: Biquad::identity(),
            conv: Convolver::new(ir),
            dry_delay: [0.0; CONV_BLOCK],
            dry_idx: 0,
            room: Room::new(sr),
        };
        s.set_params(p);
        s
    }

    pub fn set_params(&mut self, p: &SimParams) {
        let sr = self.sr;
        self.eq_fixed[0].retune(Biquad::highpass(sr, 70.0, 0.707));
        self.eq_fixed[1].retune(Biquad::peaking(sr, 800.0, 0.9, -4.0));
        self.eq_fixed[2].retune(Biquad::peaking(sr, 3500.0, 1.4, -3.0));
        self.warmth
            .retune(Biquad::peaking(sr, 110.0, 1.0, p.warmth_db));
        self.bright
            .retune(Biquad::high_shelf(sr, 5500.0, p.brightness_db));
        self.level = 10f32.powf(p.level_db / 20.0);
        self.p = p.clone();
    }

    pub fn set_ir(&mut self, ir: Arc<IrSpectrum>) {
        self.conv = Convolver::new(ir);
    }

    pub fn reset(&mut self) {
        for f in &mut self.eq_fixed {
            f.reset();
        }
        self.warmth.reset();
        self.bright.reset();
        self.conv.reset();
        self.dry_delay = [0.0; CONV_BLOCK];
        self.room.reset();
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        if !self.p.enabled {
            return x;
        }
        let mut s = x;
        if self.p.pickup_eq {
            for f in &mut self.eq_fixed {
                s = f.process(s);
            }
        }
        s = self.warmth.process(s);
        s = self.bright.process(s);

        // Dry path delayed to line up with the convolver's block latency.
        let dry = std::mem::replace(&mut self.dry_delay[self.dry_idx], s);
        self.dry_idx = (self.dry_idx + 1) % CONV_BLOCK;
        let wet = self.conv.process(s);
        s = dry * (1.0 - self.p.body) + wet * self.p.body;

        s += self.room.process(s) * self.p.room;
        s * self.level
    }
}

/// Synthesises an acoustic guitar body response: the air (Helmholtz) resonance,
/// top plate modes and a spread of decaying higher modes, plus the direct sound.
pub fn builtin_body_ir(sr: f32) -> Vec<f32> {
    // (frequency Hz, gain, decay ms)
    const MODES: &[(f32, f32, f32)] = &[
        (98.0, 1.0, 70.0),
        (196.0, 0.8, 50.0),
        (240.0, 0.5, 40.0),
        (390.0, 0.45, 32.0),
        (510.0, 0.35, 26.0),
        (730.0, 0.28, 20.0),
        (980.0, 0.3, 18.0),
        (1250.0, 0.26, 15.0),
        (1600.0, 0.24, 12.0),
        (2100.0, 0.22, 10.0),
        (2800.0, 0.2, 8.0),
        (3600.0, 0.18, 6.0),
        (4700.0, 0.15, 5.0),
        (6200.0, 0.12, 4.0),
        (8000.0, 0.08, 3.0),
    ];
    let len = (0.15 * sr) as usize;
    let mut ir = vec![0.0f32; len];
    let mut seed = 0x9E37_79B9u32;
    let mut rand = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32
    };

    for &(f, g, d) in MODES {
        let phase = rand() * 2.0 * PI;
        let tau = d / 1000.0 * sr;
        let w = 2.0 * PI * f / sr;
        for (n, v) in ir.iter_mut().enumerate() {
            let t = n as f32;
            *v += g * (-t / tau).exp() * (w * t + phase).sin();
        }
    }
    // A short burst of bright "air" noise for pick attack sparkle.
    let tau = 0.004 * sr;
    let mut prev = 0.0;
    for (n, v) in ir.iter_mut().enumerate() {
        let r = rand() * 2.0 - 1.0;
        *v += 0.35 * (r - prev) * (-(n as f32) / tau).exp();
        prev = r;
    }
    normalize_energy(&mut ir);
    // Direct sound so the attack stays defined.
    for v in &mut ir {
        *v *= 0.85;
    }
    ir[0] += 0.5;
    normalize_energy(&mut ir);
    ir
}

/// Scale so a white-noise input keeps roughly the same loudness.
pub fn normalize_energy(ir: &mut [f32]) {
    let e: f32 = ir.iter().map(|v| v * v).sum::<f32>().sqrt();
    if e > 1e-9 {
        for v in ir {
            *v /= e;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convolver_matches_direct_convolution() {
        let ir: Vec<f32> = (0..300)
            .map(|i| ((i * 7919) % 101) as f32 / 101.0 - 0.5)
            .collect();
        let x: Vec<f32> = (0..1000)
            .map(|i| ((i * 104729) % 97) as f32 / 97.0 - 0.5)
            .collect();
        let mut conv = Convolver::new(Arc::new(IrSpectrum::new("t", &ir)));
        let out: Vec<f32> = x.iter().map(|&s| conv.process(s)).collect();
        for n in 0..(x.len() - CONV_BLOCK) {
            let expected: f32 = (0..ir.len())
                .filter(|&k| k <= n)
                .map(|k| ir[k] * x[n - k])
                .sum();
            let got = out[n + CONV_BLOCK];
            assert!(
                (expected - got).abs() < 1e-3,
                "n={n} expected {expected} got {got}"
            );
        }
    }

    #[test]
    fn builtin_ir_is_sane() {
        let ir = builtin_body_ir(48000.0);
        assert!(ir.iter().all(|v| v.is_finite()));
        let e: f32 = ir.iter().map(|v| v * v).sum();
        assert!((e - 1.0).abs() < 1e-3);
    }

    #[test]
    fn sim_is_stable_on_loud_input() {
        let sr = 48000.0;
        let p = SimParams::default();
        let ir = Arc::new(IrSpectrum::new("b", &builtin_body_ir(sr)));
        let mut sim = AcousticSim::new(sr, &p, ir);
        let mut peak = 0.0f32;
        for n in 0..(sr as usize * 2) {
            let x = (2.0 * PI * 196.0 * n as f32 / sr).sin() * 0.9;
            peak = peak.max(sim.process(x).abs());
        }
        assert!(peak.is_finite() && peak < 20.0, "peak {peak}");
    }
}
