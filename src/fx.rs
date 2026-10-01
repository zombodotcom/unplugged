//! Built-in effects and effect chains.
//!
//! Every track, the input (what you monitor and record through) and the master bus
//! have a chain of [`FxSlot`]s. A slot is plain data (kind + parameter values) that
//! lives in the song and is saved with the project; the engine turns it into a
//! running [`Effect`] and keeps that effect's state when only its knobs change.

use crate::dsp::{AcousticSim, Biquad, IrSpectrum, SimParams, builtin_body_ir};
use serde::{Deserialize, Serialize};
use std::f32::consts::PI;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FxKind {
    Eq,
    Compressor,
    Gate,
    Limiter,
    Drive,
    Chorus,
    Delay,
    Reverb,
    Utility,
    Acoustic,
}

pub struct ParamDef {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub unit: &'static str,
    pub log: bool,
    /// On/off switch rather than a knob.
    pub switch: bool,
}

const fn knob(
    name: &'static str,
    min: f32,
    max: f32,
    default: f32,
    unit: &'static str,
    log: bool,
) -> ParamDef {
    ParamDef {
        name,
        min,
        max,
        default,
        unit,
        log,
        switch: false,
    }
}

const fn switch(name: &'static str, on: bool) -> ParamDef {
    ParamDef {
        name,
        min: 0.0,
        max: 1.0,
        default: if on { 1.0 } else { 0.0 },
        unit: "",
        log: false,
        switch: true,
    }
}

impl FxKind {
    pub const ALL: [FxKind; 10] = [
        FxKind::Eq,
        FxKind::Compressor,
        FxKind::Gate,
        FxKind::Limiter,
        FxKind::Drive,
        FxKind::Chorus,
        FxKind::Delay,
        FxKind::Reverb,
        FxKind::Utility,
        FxKind::Acoustic,
    ];

    pub fn name(self) -> &'static str {
        match self {
            FxKind::Eq => "EQ",
            FxKind::Compressor => "Compressor",
            FxKind::Gate => "Noise gate",
            FxKind::Limiter => "Limiter",
            FxKind::Drive => "Drive",
            FxKind::Chorus => "Chorus",
            FxKind::Delay => "Delay",
            FxKind::Reverb => "Reverb",
            FxKind::Utility => "Utility",
            FxKind::Acoustic => "Acoustic sim",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            FxKind::Eq => "Shape the tone: cut rumble, boost or cut lows, mids and highs",
            FxKind::Compressor => "Evens out loud and quiet playing",
            FxKind::Gate => "Silences hum and hiss between notes",
            FxKind::Limiter => "Stops peaks going over a ceiling (good last on the master)",
            FxKind::Drive => "Overdrive / saturation, from warm to fuzzy",
            FxKind::Chorus => "Shimmery, doubled sound",
            FxKind::Delay => "Echoes",
            FxKind::Reverb => "Room / hall ambience",
            FxKind::Utility => "Volume and stereo width",
            FxKind::Acoustic => "Makes an electric guitar DI sound like an acoustic",
        }
    }

    pub fn params(self) -> &'static [ParamDef] {
        const EQ: &[ParamDef] = &[
            knob("Low cut", 20.0, 500.0, 20.0, "Hz", true),
            knob("Low", -15.0, 15.0, 0.0, "dB", false),
            knob("Mid freq", 200.0, 8000.0, 1000.0, "Hz", true),
            knob("Mid", -15.0, 15.0, 0.0, "dB", false),
            knob("Mid width", 0.3, 4.0, 1.0, "Q", true),
            knob("High", -15.0, 15.0, 0.0, "dB", false),
        ];
        const COMP: &[ParamDef] = &[
            knob("Threshold", -60.0, 0.0, -18.0, "dB", false),
            knob("Ratio", 1.0, 20.0, 4.0, ":1", true),
            knob("Attack", 0.1, 100.0, 10.0, "ms", true),
            knob("Release", 10.0, 1000.0, 120.0, "ms", true),
            knob("Makeup", 0.0, 24.0, 0.0, "dB", false),
        ];
        const GATE: &[ParamDef] = &[
            knob("Threshold", -80.0, -10.0, -50.0, "dB", false),
            knob("Release", 10.0, 500.0, 80.0, "ms", true),
        ];
        const LIMIT: &[ParamDef] = &[
            knob("Ceiling", -12.0, 0.0, -1.0, "dB", false),
            knob("Release", 10.0, 500.0, 80.0, "ms", true),
        ];
        const DRIVE: &[ParamDef] = &[
            knob("Drive", 0.0, 40.0, 12.0, "dB", false),
            knob("Tone", 500.0, 12000.0, 4000.0, "Hz", true),
            knob("Mix", 0.0, 1.0, 1.0, "", false),
            knob("Level", -24.0, 6.0, -6.0, "dB", false),
        ];
        const CHORUS: &[ParamDef] = &[
            knob("Rate", 0.05, 5.0, 0.8, "Hz", true),
            knob("Depth", 0.0, 10.0, 3.0, "ms", false),
            knob("Mix", 0.0, 1.0, 0.5, "", false),
        ];
        const DELAY: &[ParamDef] = &[
            knob("Time", 20.0, 1500.0, 350.0, "ms", true),
            knob("Feedback", 0.0, 0.95, 0.35, "", false),
            knob("Mix", 0.0, 1.0, 0.3, "", false),
            knob("Tone", 1000.0, 16000.0, 6000.0, "Hz", true),
            switch("Ping-pong", false),
        ];
        const REVERB: &[ParamDef] = &[
            knob("Size", 0.0, 1.0, 0.6, "", false),
            knob("Damping", 0.0, 1.0, 0.4, "", false),
            knob("Mix", 0.0, 1.0, 0.25, "", false),
            knob("Width", 0.0, 1.0, 1.0, "", false),
        ];
        const UTIL: &[ParamDef] = &[
            knob("Gain", -24.0, 24.0, 0.0, "dB", false),
            knob("Width", 0.0, 2.0, 1.0, "", false),
        ];
        const ACOUSTIC: &[ParamDef] = &[
            knob("Body", 0.0, 1.0, 0.85, "", false),
            knob("Sparkle", -6.0, 14.0, 6.0, "dB", false),
            knob("Warmth", -6.0, 10.0, 3.0, "dB", false),
            knob("Room", 0.0, 1.0, 0.25, "", false),
            switch("Pickup EQ", true),
        ];
        match self {
            FxKind::Eq => EQ,
            FxKind::Compressor => COMP,
            FxKind::Gate => GATE,
            FxKind::Limiter => LIMIT,
            FxKind::Drive => DRIVE,
            FxKind::Chorus => CHORUS,
            FxKind::Delay => DELAY,
            FxKind::Reverb => REVERB,
            FxKind::Utility => UTIL,
            FxKind::Acoustic => ACOUSTIC,
        }
    }
}

/// One effect in a chain, as saved in the song.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FxSlot {
    pub id: u64,
    pub kind: FxKind,
    pub on: bool,
    pub params: Vec<f32>,
}

impl FxSlot {
    pub fn new(id: u64, kind: FxKind) -> Self {
        Self {
            id,
            kind,
            on: true,
            params: kind.params().iter().map(|p| p.default).collect(),
        }
    }

    /// Parameter values, filled with defaults if the saved list is short (older files).
    pub fn values(&self) -> Vec<f32> {
        self.kind
            .params()
            .iter()
            .enumerate()
            .map(|(i, d)| {
                self.params
                    .get(i)
                    .copied()
                    .unwrap_or(d.default)
                    .clamp(d.min, d.max)
            })
            .collect()
    }
}

pub trait Effect: Send {
    fn set(&mut self, p: &[f32]);
    fn process(&mut self, l: &mut f32, r: &mut f32);
    fn reset(&mut self);
    /// Samples of delay this effect adds.
    fn latency(&self) -> usize {
        0
    }
}

pub fn build(kind: FxKind, sr: f32) -> Box<dyn Effect> {
    match kind {
        FxKind::Eq => Box::new(Eq::new(sr)),
        FxKind::Compressor => Box::new(Compressor::new(sr)),
        FxKind::Gate => Box::new(Gate::new(sr)),
        FxKind::Limiter => Box::new(Limiter::new(sr)),
        FxKind::Drive => Box::new(Drive::new(sr)),
        FxKind::Chorus => Box::new(Chorus::new(sr)),
        FxKind::Delay => Box::new(Delay::new(sr)),
        FxKind::Reverb => Box::new(Reverb::new(sr)),
        FxKind::Utility => Box::new(Utility::default()),
        FxKind::Acoustic => Box::new(Acoustic::new(sr)),
    }
}

/// A running chain of effects.
pub struct Chain {
    sr: f32,
    slots: Vec<(u64, FxKind, bool, Box<dyn Effect>)>,
}

impl Chain {
    pub fn new(sr: f32) -> Self {
        Self {
            sr,
            slots: Vec::new(),
        }
    }

    pub fn from_slots(sr: f32, slots: &[FxSlot]) -> Self {
        let mut c = Self::new(sr);
        c.sync(slots);
        c
    }

    /// Match `slots`, keeping the running state of effects that are still there.
    pub fn sync(&mut self, slots: &[FxSlot]) {
        let mut old = std::mem::take(&mut self.slots);
        for s in slots {
            let mut fx = match old.iter().position(|o| o.0 == s.id && o.1 == s.kind) {
                Some(i) => old.swap_remove(i).3,
                None => build(s.kind, self.sr),
            };
            fx.set(&s.values());
            self.slots.push((s.id, s.kind, s.on, fx));
        }
    }

    #[inline]
    pub fn process(&mut self, l: &mut f32, r: &mut f32) {
        for (_, _, on, fx) in &mut self.slots {
            if *on {
                fx.process(l, r);
            }
        }
    }

    pub fn reset(&mut self) {
        for s in &mut self.slots {
            s.3.reset();
        }
    }

    pub fn latency(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.2)
            .map(|s| s.3.latency())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|s| !s.2)
    }
}

fn db(v: f32) -> f32 {
    10f32.powf(v / 20.0)
}

fn coef(ms: f32, sr: f32) -> f32 {
    (-1.0 / (ms.max(0.01) * 0.001 * sr)).exp()
}

// ---------------------------------------------------------------------------

struct Eq {
    sr: f32,
    f: [[Biquad; 4]; 2],
    low_cut: bool,
}

impl Eq {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            f: [[Biquad::identity(); 4]; 2],
            low_cut: false,
        }
    }
}

impl Effect for Eq {
    fn set(&mut self, p: &[f32]) {
        let sr = self.sr;
        self.low_cut = p[0] > 21.0;
        for ch in &mut self.f {
            ch[0].retune(Biquad::highpass(sr, p[0], 0.707));
            ch[1].retune(Biquad::low_shelf(sr, 120.0, p[1]));
            ch[2].retune(Biquad::peaking(sr, p[2], p[4], p[3]));
            ch[3].retune(Biquad::high_shelf(sr, 8000.0, p[5]));
        }
    }

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        for (x, f) in [l, r].into_iter().zip(&mut self.f) {
            let mut s = *x;
            if self.low_cut {
                s = f[0].process(s);
            }
            s = f[1].process(s);
            s = f[2].process(s);
            *x = f[3].process(s);
        }
    }

    fn reset(&mut self) {
        self.f.iter_mut().flatten().for_each(Biquad::reset);
    }
}

struct Compressor {
    sr: f32,
    thr: f32,
    slope: f32,
    att: f32,
    rel: f32,
    makeup: f32,
    env_db: f32,
}

impl Compressor {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            thr: -18.0,
            slope: 0.75,
            att: 0.0,
            rel: 0.0,
            makeup: 1.0,
            env_db: -120.0,
        }
    }
}

impl Effect for Compressor {
    fn set(&mut self, p: &[f32]) {
        self.thr = p[0];
        self.slope = 1.0 - 1.0 / p[1].max(1.0);
        self.att = coef(p[2], self.sr);
        self.rel = coef(p[3], self.sr);
        self.makeup = db(p[4]);
    }

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        let peak = l.abs().max(r.abs());
        let x_db = 20.0 * (peak + 1e-9).log10();
        let c = if x_db > self.env_db {
            self.att
        } else {
            self.rel
        };
        self.env_db = x_db + c * (self.env_db - x_db);
        let over = self.env_db - self.thr;
        let g = if over > 0.0 {
            db(-over * self.slope)
        } else {
            1.0
        } * self.makeup;
        *l *= g;
        *r *= g;
    }

    fn reset(&mut self) {
        self.env_db = -120.0;
    }
}

struct Gate {
    sr: f32,
    thr: f32,
    env: f32,
    gain: f32,
    env_rel: f32,
    open: f32,
    close: f32,
}

impl Gate {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            thr: db(-50.0),
            env: 0.0,
            gain: 0.0,
            env_rel: coef(20.0, sr),
            open: coef(1.0, sr),
            close: 0.0,
        }
    }
}

impl Effect for Gate {
    fn set(&mut self, p: &[f32]) {
        self.thr = db(p[0]);
        self.close = coef(p[1], self.sr);
    }

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        let peak = l.abs().max(r.abs());
        self.env = peak.max(self.env * self.env_rel);
        let target = if self.env > self.thr { 1.0 } else { 0.0 };
        let c = if target > self.gain {
            self.open
        } else {
            self.close
        };
        self.gain = target + c * (self.gain - target);
        *l *= self.gain;
        *r *= self.gain;
    }

    fn reset(&mut self) {
        self.env = 0.0;
        self.gain = 0.0;
    }
}

struct Limiter {
    sr: f32,
    ceiling: f32,
    rel: f32,
    env: f32,
}

impl Limiter {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            ceiling: db(-1.0),
            rel: 0.0,
            env: 0.0,
        }
    }
}

impl Effect for Limiter {
    fn set(&mut self, p: &[f32]) {
        self.ceiling = db(p[0]);
        self.rel = coef(p[1], self.sr);
    }

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        // Instant attack: the gain drops on the very sample that would go over.
        let peak = l.abs().max(r.abs());
        self.env = peak.max(self.env * self.rel);
        if self.env > self.ceiling {
            let g = self.ceiling / self.env;
            *l *= g;
            *r *= g;
        }
    }

    fn reset(&mut self) {
        self.env = 0.0;
    }
}

struct Drive {
    sr: f32,
    gain: f32,
    norm: f32,
    mix: f32,
    level: f32,
    tone: [Biquad; 2],
}

impl Drive {
    fn new(sr: f32) -> Self {
        Self {
            sr,
            gain: 1.0,
            norm: 1.0,
            mix: 1.0,
            level: 1.0,
            tone: [Biquad::identity(); 2],
        }
    }
}

impl Effect for Drive {
    fn set(&mut self, p: &[f32]) {
        self.gain = db(p[0]);
        // Keep loud input roughly level-matched as the drive goes up.
        self.norm = 1.0 / self.gain.sqrt();
        for t in &mut self.tone {
            t.retune(Biquad::lowpass(self.sr, p[1], 0.707));
        }
        self.mix = p[2];
        self.level = db(p[3]);
    }

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        for (x, t) in [l, r].into_iter().zip(&mut self.tone) {
            let wet = t.process((*x * self.gain).tanh() * self.norm.max(0.3));
            *x = (*x * (1.0 - self.mix) + wet * self.mix) * self.level;
        }
    }

    fn reset(&mut self) {
        self.tone.iter_mut().for_each(Biquad::reset);
    }
}

/// A delay line with fractional (linearly interpolated) reads.
struct Line {
    buf: Vec<f32>,
    pos: usize,
}

impl Line {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0.0; len.max(2)],
            pos: 0,
        }
    }

    #[inline]
    fn read(&self, delay: f32) -> f32 {
        let n = self.buf.len();
        let d = delay.clamp(1.0, (n - 2) as f32);
        let i = d as usize;
        let f = d - i as f32;
        let a = self.buf[(self.pos + n - i) % n];
        let b = self.buf[(self.pos + n - i - 1) % n];
        a + (b - a) * f
    }

    #[inline]
    fn write(&mut self, x: f32) {
        self.pos = (self.pos + 1) % self.buf.len();
        self.buf[self.pos] = x;
    }

    fn clear(&mut self) {
        self.buf.fill(0.0);
    }
}

struct Chorus {
    sr: f32,
    lines: [Line; 2],
    phase: f32,
    inc: f32,
    depth: f32,
    mix: f32,
}

impl Chorus {
    fn new(sr: f32) -> Self {
        let len = (0.05 * sr) as usize;
        Self {
            sr,
            lines: [Line::new(len), Line::new(len)],
            phase: 0.0,
            inc: 0.0,
            depth: 0.0,
            mix: 0.5,
        }
    }
}

impl Effect for Chorus {
    fn set(&mut self, p: &[f32]) {
        self.inc = 2.0 * PI * p[0] / self.sr;
        self.depth = p[1] * 0.001 * self.sr;
        self.mix = p[2];
    }

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        let base = 0.012 * self.sr;
        self.phase = (self.phase + self.inc) % (2.0 * PI);
        for (i, (x, line)) in [l, r].into_iter().zip(&mut self.lines).enumerate() {
            line.write(*x);
            let d = base + self.depth * 0.5 * (1.0 + (self.phase + i as f32 * PI / 2.0).sin());
            let wet = line.read(d);
            *x = *x * (1.0 - self.mix * 0.5) + wet * self.mix;
        }
    }

    fn reset(&mut self) {
        self.lines.iter_mut().for_each(Line::clear);
    }
}

struct Delay {
    sr: f32,
    lines: [Line; 2],
    time: f32,
    fb: f32,
    mix: f32,
    tone: [Biquad; 2],
    ping_pong: bool,
}

impl Delay {
    fn new(sr: f32) -> Self {
        let len = (1.6 * sr) as usize;
        Self {
            sr,
            lines: [Line::new(len), Line::new(len)],
            time: 0.35 * sr,
            fb: 0.35,
            mix: 0.3,
            tone: [Biquad::identity(); 2],
            ping_pong: false,
        }
    }
}

impl Effect for Delay {
    fn set(&mut self, p: &[f32]) {
        self.time = p[0] * 0.001 * self.sr;
        self.fb = p[1];
        self.mix = p[2];
        for t in &mut self.tone {
            t.retune(Biquad::lowpass(self.sr, p[3], 0.707));
        }
        self.ping_pong = p[4] > 0.5;
    }

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        let yl = self.tone[0].process(self.lines[0].read(self.time));
        let yr = self.tone[1].process(self.lines[1].read(self.time));
        if self.ping_pong {
            self.lines[0].write((*l + *r) * 0.5 + yr * self.fb);
            self.lines[1].write(yl * self.fb);
        } else {
            self.lines[0].write(*l + yl * self.fb);
            self.lines[1].write(*r + yr * self.fb);
        }
        *l += yl * self.mix;
        *r += yr * self.mix;
    }

    fn reset(&mut self) {
        self.lines.iter_mut().for_each(Line::clear);
        self.tone.iter_mut().for_each(Biquad::reset);
    }
}

struct Comb {
    buf: Vec<f32>,
    idx: usize,
    store: f32,
}

impl Comb {
    #[inline]
    fn process(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let y = self.buf[self.idx];
        self.store = y * (1.0 - damp) + self.store * damp;
        self.buf[self.idx] = x + self.store * feedback;
        self.idx = (self.idx + 1) % self.buf.len();
        y
    }
}

struct Allpass {
    buf: Vec<f32>,
    idx: usize,
}

impl Allpass {
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let b = self.buf[self.idx];
        self.buf[self.idx] = x + b * 0.5;
        self.idx = (self.idx + 1) % self.buf.len();
        b - x
    }
}

/// Stereo Freeverb (Jezar's public-domain design).
struct Reverb {
    combs: [Vec<Comb>; 2],
    aps: [Vec<Allpass>; 2],
    feedback: f32,
    damp: f32,
    wet1: f32,
    wet2: f32,
}

impl Reverb {
    fn new(sr: f32) -> Self {
        let k = sr / 44100.0;
        let len = |n: usize| ((n as f32 * k) as usize).max(1);
        let combs = |spread: usize| {
            [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617]
                .iter()
                .map(|&n| Comb {
                    buf: vec![0.0; len(n + spread)],
                    idx: 0,
                    store: 0.0,
                })
                .collect()
        };
        let aps = |spread: usize| {
            [556, 441, 341, 225]
                .iter()
                .map(|&n| Allpass {
                    buf: vec![0.0; len(n + spread)],
                    idx: 0,
                })
                .collect()
        };
        Self {
            combs: [combs(0), combs(23)],
            aps: [aps(0), aps(23)],
            feedback: 0.84,
            damp: 0.2,
            wet1: 0.0,
            wet2: 0.0,
        }
    }
}

impl Effect for Reverb {
    fn set(&mut self, p: &[f32]) {
        self.feedback = 0.7 + p[0] * 0.28;
        self.damp = p[1] * 0.4;
        let wet = p[2] * 3.0;
        self.wet1 = wet * (p[3] / 2.0 + 0.5);
        self.wet2 = wet * ((1.0 - p[3]) / 2.0);
    }

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        let input = (*l + *r) * 0.015;
        let mut out = [0.0f32; 2];
        for ch in 0..2 {
            let mut s = 0.0;
            for c in &mut self.combs[ch] {
                s += c.process(input, self.feedback, self.damp);
            }
            for a in &mut self.aps[ch] {
                s = a.process(s);
            }
            out[ch] = s;
        }
        *l += out[0] * self.wet1 + out[1] * self.wet2;
        *r += out[1] * self.wet1 + out[0] * self.wet2;
    }

    fn reset(&mut self) {
        for c in self.combs.iter_mut().flatten() {
            c.buf.fill(0.0);
            c.store = 0.0;
        }
        for a in self.aps.iter_mut().flatten() {
            a.buf.fill(0.0);
        }
    }
}

#[derive(Default)]
struct Utility {
    gain: f32,
    width: f32,
}

impl Effect for Utility {
    fn set(&mut self, p: &[f32]) {
        self.gain = db(p[0]);
        self.width = p[1];
    }

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        let mid = (*l + *r) * 0.5;
        let side = (*l - *r) * 0.5 * self.width;
        *l = (mid + side) * self.gain;
        *r = (mid - side) * self.gain;
    }

    fn reset(&mut self) {}
}

/// The electric-to-acoustic simulator, as an ordinary effect.
struct Acoustic {
    sim: AcousticSim,
}

impl Acoustic {
    fn new(sr: f32) -> Self {
        let ir = Arc::new(IrSpectrum::new("Built-in body", &builtin_body_ir(sr)));
        Self {
            sim: AcousticSim::new(sr, &SimParams::default(), ir),
        }
    }
}

impl Effect for Acoustic {
    fn set(&mut self, p: &[f32]) {
        self.sim.set_params(&SimParams {
            enabled: true,
            pickup_eq: p[4] > 0.5,
            body: p[0],
            brightness_db: p[1],
            warmth_db: p[2],
            room: p[3],
            level_db: 0.0,
            ir_path: None,
        });
    }

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        let y = self.sim.process((*l + *r) * 0.5);
        *l = y;
        *r = y;
    }

    fn reset(&mut self) {
        self.sim.reset();
    }

    fn latency(&self) -> usize {
        crate::dsp::CONV_BLOCK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    fn noise(n: usize, amp: f32) -> Vec<f32> {
        let mut s = 0x1234_5678u32;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    fn run(kind: FxKind, params: Option<Vec<f32>>, input: &[f32]) -> Vec<f32> {
        let mut slot = FxSlot::new(1, kind);
        if let Some(p) = params {
            slot.params = p;
        }
        let mut c = Chain::from_slots(SR, &[slot]);
        input
            .iter()
            .map(|&x| {
                let (mut l, mut r) = (x, x);
                c.process(&mut l, &mut r);
                (l + r) * 0.5
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn every_effect_is_stable_on_loud_noise() {
        let input = noise(SR as usize, 0.9);
        for kind in FxKind::ALL {
            let out = run(kind, None, &input);
            assert!(
                out.iter().all(|v| v.is_finite() && v.abs() < 20.0),
                "{kind:?} blew up"
            );
        }
    }

    #[test]
    fn flat_eq_passes_audio_unchanged() {
        let input = noise(4800, 0.5);
        let out = run(FxKind::Eq, None, &input);
        for (a, b) in input.iter().zip(&out) {
            assert!((a - b).abs() < 1e-3);
        }
    }

    #[test]
    fn compressor_turns_loud_parts_down() {
        let input = noise(SR as usize, 0.9);
        let out = run(
            FxKind::Compressor,
            Some(vec![-30.0, 10.0, 1.0, 100.0, 0.0]),
            &input,
        );
        assert!(rms(&out[24000..]) < rms(&input[24000..]) * 0.5);
    }

    #[test]
    fn limiter_holds_the_ceiling() {
        let input = noise(SR as usize, 1.5);
        let out = run(FxKind::Limiter, Some(vec![-6.0, 50.0]), &input);
        let ceiling = db(-6.0);
        assert!(out.iter().all(|v| v.abs() <= ceiling + 1e-4));
    }

    #[test]
    fn gate_silences_quiet_hiss() {
        let input = noise(SR as usize, db(-70.0));
        let out = run(FxKind::Gate, None, &input);
        assert!(rms(&out[24000..]) < 1e-6);
    }

    #[test]
    fn delay_echoes_after_its_time() {
        let mut input = vec![0.0; SR as usize];
        input[0] = 1.0;
        let out = run(
            FxKind::Delay,
            Some(vec![100.0, 0.0, 1.0, 16000.0, 0.0]),
            &input,
        );
        let echo = out.iter().skip(10).position(|v| v.abs() > 0.3).unwrap() + 10;
        assert!((echo as i64 - 4800).abs() < 10, "echo at {echo}");
    }

    #[test]
    fn chain_keeps_state_when_knobs_move() {
        let mut slot = FxSlot::new(7, FxKind::Delay);
        let mut c = Chain::from_slots(SR, std::slice::from_ref(&slot));
        let (mut l, mut r) = (1.0, 1.0);
        c.process(&mut l, &mut r);
        slot.params[2] = 0.9; // turn the mix up: the impulse already in the line must survive
        c.sync(std::slice::from_ref(&slot));
        let mut heard = false;
        for _ in 0..(0.4 * SR) as usize {
            let (mut l, mut r) = (0.0, 0.0);
            c.process(&mut l, &mut r);
            heard |= l.abs() > 0.05;
        }
        assert!(heard);
    }
}
