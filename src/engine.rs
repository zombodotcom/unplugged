//! The real-time engine: live monitoring, track playback, recording and the metronome.
//!
//! The engine lives behind a mutex. The audio thread only ever `try_lock`s it and the
//! UI thread keeps its critical sections tiny (heavy work like building tracks or
//! loading IRs happens outside the lock).

use crate::dsp::{AcousticSim, IrSpectrum, SimParams, builtin_body_ir};
use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::Arc;

/// Samples per waveform peak bucket.
pub const PEAK_BUCKET: usize = 256;

pub struct Track {
    pub id: u64,
    pub name: String,
    pub samples: Arc<Vec<f32>>,
    pub peaks: Arc<Vec<[f32; 2]>>,
    /// Position of the first sample on the timeline, in samples.
    pub start: usize,
    pub volume: f32,
    /// -1 = left, 0 = centre, 1 = right.
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    /// Run this track through the acoustic simulator on playback.
    pub acoustic: bool,
    sim: AcousticSim,
}

/// Plain copy of a track's settings, for the UI and for saving.
#[derive(Clone)]
pub struct TrackView {
    pub id: u64,
    pub name: String,
    pub samples: Arc<Vec<f32>>,
    pub peaks: Arc<Vec<[f32; 2]>>,
    pub start: usize,
    pub volume: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub acoustic: bool,
}

impl TrackView {
    pub fn end(&self) -> usize {
        self.start + self.samples.len()
    }
}

pub fn compute_peaks(samples: &[f32]) -> Vec<[f32; 2]> {
    samples
        .chunks(PEAK_BUCKET)
        .map(|c| {
            c.iter()
                .fold([0.0f32, 0.0f32], |[lo, hi], &s| [lo.min(s), hi.max(s)])
        })
        .collect()
}

pub struct Engine {
    pub sr: f32,
    tracks: Vec<Track>,
    next_id: u64,

    pub playing: bool,
    pub playhead: usize,
    pub recording: bool,
    rec_buf: Vec<f32>,
    rec_start: usize,

    pub monitor: bool,
    pub input_gain: f32,
    pub master: f32,
    pub metronome: bool,
    pub bpm: f32,
    pub click_volume: f32,
    /// Round-trip latency to compensate recordings by, in milliseconds.
    pub latency_ms: f32,
    /// Default "acoustic" flag for new recordings.
    pub record_acoustic: bool,

    params: SimParams,
    ir: Arc<IrSpectrum>,
    monitor_sim: AcousticSim,

    /// Peak meters, reset by the UI when read.
    pub in_peak: f32,
    pub out_peak: f32,
}

impl Engine {
    pub fn new(sr: f32) -> Self {
        let params = SimParams::default();
        let ir = Arc::new(IrSpectrum::new("Built-in body", &builtin_body_ir(sr)));
        Self {
            sr,
            tracks: Vec::new(),
            next_id: 1,
            playing: false,
            playhead: 0,
            recording: false,
            rec_buf: Vec::new(),
            rec_start: 0,
            monitor: true,
            input_gain: 1.0,
            master: 0.8,
            metronome: false,
            bpm: 90.0,
            click_volume: 0.4,
            latency_ms: 12.0,
            record_acoustic: true,
            monitor_sim: AcousticSim::new(sr, &params, ir.clone()),
            params,
            ir,
            in_peak: 0.0,
            out_peak: 0.0,
        }
    }

    // ---- settings --------------------------------------------------------

    pub fn ir_name(&self) -> &str {
        &self.ir.name
    }

    pub fn set_params(&mut self, p: &SimParams) {
        self.monitor_sim.set_params(p);
        for t in &mut self.tracks {
            t.sim.set_params(p);
        }
        self.params = p.clone();
    }

    /// Swap in a new IR for the monitor and every track.
    pub fn set_ir(&mut self, ir: Arc<IrSpectrum>) {
        self.monitor_sim.set_ir(ir.clone());
        for t in &mut self.tracks {
            t.sim.set_ir(ir.clone());
        }
        self.ir = ir;
    }

    pub fn ir(&self) -> Arc<IrSpectrum> {
        self.ir.clone()
    }

    /// Called when the audio device (re)starts. Rebuilds processors if the rate changed.
    pub fn set_sample_rate(&mut self, sr: f32) {
        if (sr - self.sr).abs() < 0.5 {
            return;
        }
        self.sr = sr;
        if self.params.ir_path.is_none() {
            self.ir = Arc::new(IrSpectrum::new("Built-in body", &builtin_body_ir(sr)));
        }
        self.monitor_sim = AcousticSim::new(sr, &self.params, self.ir.clone());
        for t in &mut self.tracks {
            t.sim = AcousticSim::new(sr, &self.params, self.ir.clone());
        }
    }

    // ---- tracks ----------------------------------------------------------

    pub fn new_sim(&self) -> AcousticSim {
        AcousticSim::new(self.sr, &self.params, self.ir.clone())
    }

    pub fn views(&self) -> Vec<TrackView> {
        self.tracks
            .iter()
            .map(|t| TrackView {
                id: t.id,
                name: t.name.clone(),
                samples: t.samples.clone(),
                peaks: t.peaks.clone(),
                start: t.start,
                volume: t.volume,
                pan: t.pan,
                mute: t.mute,
                solo: t.solo,
                acoustic: t.acoustic,
            })
            .collect()
    }

    /// Add a track. `sim` should come from [`Engine::new_sim`] (built outside the lock).
    pub fn add_track(&mut self, mut v: TrackView, sim: AcousticSim) -> u64 {
        v.id = self.next_id;
        self.next_id += 1;
        self.tracks.push(Track {
            id: v.id,
            name: v.name,
            samples: v.samples,
            peaks: v.peaks,
            start: v.start,
            volume: v.volume,
            pan: v.pan,
            mute: v.mute,
            solo: v.solo,
            acoustic: v.acoustic,
            sim,
        });
        v.id
    }

    /// Apply edited settings from the UI (name, volume, pan, mute, solo, acoustic, start).
    pub fn update_track(&mut self, v: &TrackView) {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == v.id) {
            t.name.clone_from(&v.name);
            t.start = v.start;
            t.volume = v.volume;
            t.pan = v.pan;
            t.mute = v.mute;
            t.solo = v.solo;
            if t.acoustic != v.acoustic {
                t.sim.reset();
            }
            t.acoustic = v.acoustic;
        }
    }

    pub fn remove_track(&mut self, id: u64) {
        self.tracks.retain(|t| t.id != id);
    }

    pub fn clear_tracks(&mut self) {
        self.tracks.clear();
    }

    // ---- transport -------------------------------------------------------

    pub fn play(&mut self) {
        self.playing = true;
    }

    /// Stops playback. If recording, returns the take (already latency-compensated)
    /// as `(start, samples)`.
    pub fn stop(&mut self) -> Option<(usize, Vec<f32>)> {
        self.playing = false;
        self.reset_sims();
        if !self.recording {
            return None;
        }
        self.recording = false;
        let mut take = std::mem::take(&mut self.rec_buf);
        let lat = (self.latency_ms / 1000.0 * self.sr) as usize;
        let start = if lat <= self.rec_start {
            self.rec_start - lat
        } else {
            let cut = (lat - self.rec_start).min(take.len());
            take.drain(..cut);
            0
        };
        (!take.is_empty()).then_some((start, take))
    }

    /// Starts recording a new layer at the playhead (and starts playback).
    /// `buf` should be pre-allocated by the caller so the audio thread rarely allocates.
    pub fn record(&mut self, buf: Vec<f32>) {
        self.rec_buf = buf;
        self.rec_buf.clear();
        self.rec_start = self.playhead;
        self.recording = true;
        self.playing = true;
    }

    pub fn recording_len(&self) -> usize {
        self.rec_buf.len()
    }

    pub fn recording_start(&self) -> usize {
        self.rec_start
    }

    pub fn seek(&mut self, pos: usize) {
        if self.recording {
            return;
        }
        self.playhead = pos;
        self.reset_sims();
    }

    fn reset_sims(&mut self) {
        for t in &mut self.tracks {
            t.sim.reset();
        }
    }

    // ---- audio -----------------------------------------------------------

    fn click(&self, pos: usize) -> f32 {
        let beat = (self.sr * 60.0 / self.bpm.max(20.0)) as usize;
        let beat_idx = pos / beat;
        let t = (pos % beat) as f32;
        let len = 0.03 * self.sr;
        if t > len {
            return 0.0;
        }
        let freq = if beat_idx.is_multiple_of(4) {
            1760.0
        } else {
            1175.0
        };
        (2.0 * PI * freq * t / self.sr).sin() * (-t / (0.006 * self.sr)).exp() * self.click_volume
    }

    /// Render `out.len() / 2` stereo frames. Consumes one mono input sample per frame.
    pub fn render(&mut self, input: &mut VecDeque<f32>, out: &mut [f32]) {
        let any_solo = self.tracks.iter().any(|t| t.solo);
        for frame in out.chunks_exact_mut(2) {
            let x = input.pop_front().unwrap_or(0.0) * self.input_gain;
            self.in_peak = self.in_peak.max(x.abs());

            if self.recording {
                self.rec_buf.push(x);
            }

            let mut l = 0.0;
            let mut r = 0.0;
            if self.monitor {
                let m = self.monitor_sim.process(x);
                l += m;
                r += m;
            }

            if self.playing {
                let pos = self.playhead;
                for t in &mut self.tracks {
                    let audible = !t.mute && (!any_solo || t.solo);
                    let s = if pos >= t.start && pos < t.start + t.samples.len() {
                        t.samples[pos - t.start]
                    } else {
                        0.0
                    };
                    // Keep the sim running a bit past the clip end so its tail rings out.
                    let s = if t.acoustic { t.sim.process(s) } else { s };
                    if audible {
                        let angle = (t.pan + 1.0) * PI / 4.0;
                        l += s * t.volume * angle.cos();
                        r += s * t.volume * angle.sin();
                    }
                }
                if self.metronome {
                    let c = self.click(pos);
                    l += c;
                    r += c;
                }
                self.playhead += 1;
            }

            let l = soft_clip(l * self.master);
            let r = soft_clip(r * self.master);
            self.out_peak = self.out_peak.max(l.abs()).max(r.abs());
            frame[0] = l;
            frame[1] = r;
        }
    }
}

#[inline]
fn soft_clip(x: f32) -> f32 {
    if x.abs() < 0.9 {
        x
    } else {
        x.signum() * (0.9 + 0.1 * ((x.abs() - 0.9) * 10.0).tanh())
    }
}

/// Offline mixdown of the whole session to interleaved stereo.
pub fn render_mix(
    tracks: &[TrackView],
    params: &SimParams,
    ir: Arc<IrSpectrum>,
    sr: f32,
) -> Vec<f32> {
    let tail = (sr * 2.0) as usize;
    let len = tracks.iter().map(|t| t.end()).max().unwrap_or(0) + tail;
    let lat = crate::dsp::CONV_BLOCK;
    let any_solo = tracks.iter().any(|t| t.solo);
    let mut mix = vec![0.0f32; len * 2];
    for t in tracks.iter().filter(|t| !t.mute && (!any_solo || t.solo)) {
        let angle = (t.pan + 1.0) * PI / 4.0;
        let (gl, gr) = (t.volume * angle.cos(), t.volume * angle.sin());
        let mut sim = AcousticSim::new(sr, params, ir.clone());
        let n = t.samples.len() + tail;
        for i in 0..n + lat {
            let s = t.samples.get(i).copied().unwrap_or(0.0);
            let (y, at) = if t.acoustic && params.enabled {
                (sim.process(s), i.checked_sub(lat))
            } else {
                (s, Some(i))
            };
            if let Some(at) = at.map(|a| a + t.start).filter(|&a| a < len) {
                mix[at * 2] += y * gl;
                mix[at * 2 + 1] += y * gr;
            }
        }
    }
    // Trim silent tail.
    let last = mix
        .chunks(2)
        .rposition(|f| f[0].abs() > 1e-4 || f[1].abs() > 1e-4)
        .map_or(0, |p| p + 1);
    mix.truncate(last * 2);
    let peak = mix.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
    if peak > 0.99 {
        for v in &mut mix {
            *v *= 0.99 / peak;
        }
    }
    mix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_compensates_latency() {
        let mut e = Engine::new(48000.0);
        e.monitor = false;
        e.latency_ms = 10.0; // 480 samples
        e.seek(1000);
        e.record(Vec::with_capacity(4096));
        let mut input: VecDeque<f32> = (0..2000).map(|i| i as f32).collect();
        let mut out = vec![0.0; 4000];
        e.render(&mut input, &mut out);
        let (start, take) = e.stop().unwrap();
        assert_eq!(start, 520);
        assert_eq!(take.len(), 2000);
        assert_eq!(e.playhead, 3000);
    }

    #[test]
    fn latency_trims_take_at_zero() {
        let mut e = Engine::new(48000.0);
        e.latency_ms = 10.0;
        e.record(Vec::new());
        let mut input: VecDeque<f32> = (0..1000).map(|i| i as f32).collect();
        let mut out = vec![0.0; 2000];
        e.render(&mut input, &mut out);
        let (start, take) = e.stop().unwrap();
        assert_eq!(start, 0);
        assert_eq!(take.len(), 520);
        assert_eq!(take[0], 480.0);
    }

    #[test]
    fn plays_back_tracks() {
        let mut e = Engine::new(48000.0);
        e.monitor = false;
        e.master = 1.0;
        let samples = Arc::new(vec![0.5f32; 100]);
        let sim = e.new_sim();
        e.add_track(
            TrackView {
                id: 0,
                name: "t".into(),
                peaks: Arc::new(compute_peaks(&samples)),
                samples,
                start: 10,
                volume: 1.0,
                pan: 0.0,
                mute: false,
                solo: false,
                acoustic: false,
            },
            sim,
        );
        e.play();
        let mut out = vec![0.0; 400];
        e.render(&mut VecDeque::new(), &mut out);
        assert_eq!(out[0], 0.0);
        assert!((out[20] - 0.5 * (PI / 4.0).cos()).abs() < 1e-5);
    }
}
