//! The real-time engine: live monitoring, track playback, recording and the metronome.
//!
//! The engine lives behind a mutex. The audio thread only ever `try_lock`s it and the
//! UI thread keeps its critical sections tiny. The UI owns the song ([`crate::model::Doc`])
//! and pushes a copy here with [`Engine::set_tracks`] whenever it changes; clips share
//! their audio through `Arc`s, so that copy is cheap.

use crate::fx::{Chain, FxSlot};
use crate::model::Track;
use std::collections::VecDeque;
use std::f32::consts::PI;

struct EngineTrack {
    data: Track,
    chain: Chain,
}

pub struct Engine {
    pub sr: f32,
    tracks: Vec<EngineTrack>,

    pub playing: bool,
    pub playhead: usize,
    pub recording: bool,
    rec_buf: Vec<f32>,
    rec_start: usize,
    /// Samples of count-in left before recording really starts.
    count_in: usize,
    count_in_total: usize,

    pub monitor: bool,
    pub input_gain: f32,
    pub master: f32,
    pub metronome: bool,
    pub bpm: f32,
    pub click_volume: f32,
    /// Round-trip latency to compensate recordings by, in milliseconds.
    pub latency_ms: f32,

    /// Effects you hear (and play) through live.
    input_fx: Vec<FxSlot>,
    input_chain: Chain,
    master_fx: Vec<FxSlot>,
    master_chain: Chain,

    /// The last [`CAPTURE_SECONDS`] of input, always recording (for "Capture").
    history: Vec<f32>,
    hist_pos: usize,
    hist_len: usize,

    /// Peak meters, reset by the UI when read.
    pub in_peak: f32,
    pub out_peak: f32,
}

impl Engine {
    pub fn new(sr: f32) -> Self {
        Self {
            sr,
            tracks: Vec::new(),
            playing: false,
            playhead: 0,
            recording: false,
            rec_buf: Vec::new(),
            rec_start: 0,
            count_in: 0,
            count_in_total: 0,
            monitor: true,
            input_gain: 1.0,
            master: 0.8,
            metronome: false,
            bpm: 90.0,
            click_volume: 0.4,
            latency_ms: 12.0,
            input_fx: Vec::new(),
            input_chain: Chain::new(sr),
            master_fx: Vec::new(),
            master_chain: Chain::new(sr),
            history: vec![0.0; (CAPTURE_SECONDS * sr) as usize],
            hist_pos: 0,
            hist_len: 0,
            in_peak: 0.0,
            out_peak: 0.0,
        }
    }

    // ---- settings --------------------------------------------------------

    pub fn set_input_fx(&mut self, fx: &[FxSlot]) {
        self.input_chain.sync(fx);
        self.input_fx = fx.to_vec();
    }

    pub fn set_master_fx(&mut self, fx: &[FxSlot]) {
        self.master_chain.sync(fx);
        self.master_fx = fx.to_vec();
    }

    /// Called when the audio device (re)starts. Rebuilds processors if the rate changed.
    pub fn set_sample_rate(&mut self, sr: f32) {
        if (sr - self.sr).abs() < 0.5 {
            return;
        }
        self.sr = sr;
        self.input_chain = Chain::from_slots(sr, &self.input_fx);
        self.master_chain = Chain::from_slots(sr, &self.master_fx);
        for t in &mut self.tracks {
            t.chain = Chain::from_slots(sr, &t.data.fx);
        }
        self.history = vec![0.0; (CAPTURE_SECONDS * sr) as usize];
        self.hist_pos = 0;
        self.hist_len = 0;
    }

    /// The last `seconds` of input (oldest first), whether or not you were recording.
    pub fn captured(&self, seconds: f32) -> Vec<f32> {
        let n = ((seconds * self.sr) as usize).min(self.hist_len);
        let cap = self.history.len();
        let start = (self.hist_pos + cap - n) % cap;
        (0..n).map(|i| self.history[(start + i) % cap]).collect()
    }

    // ---- tracks ----------------------------------------------------------

    /// Replace the playing song with `tracks`, keeping each track's effect state.
    pub fn set_tracks(&mut self, tracks: &[Track]) {
        let mut old: Vec<EngineTrack> = std::mem::take(&mut self.tracks);
        for t in tracks {
            let chain = match old.iter().position(|o| o.data.id == t.id) {
                Some(i) => {
                    let mut c = old.swap_remove(i).chain;
                    c.sync(&t.fx);
                    c
                }
                None => Chain::from_slots(self.sr, &t.fx),
            };
            self.tracks.push(EngineTrack {
                data: t.clone(),
                chain,
            });
        }
    }

    // ---- transport -------------------------------------------------------

    pub fn play(&mut self) {
        self.playing = true;
    }

    /// Stops playback. If recording, returns the take (already latency-compensated)
    /// as `(start, samples)`.
    pub fn stop(&mut self) -> Option<(usize, Vec<f32>)> {
        self.playing = false;
        self.count_in = 0;
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

    /// Starts recording a new layer at the playhead (and starts playback), after
    /// `count_in_beats` clicks. `buf` should be pre-allocated by the caller so the
    /// audio thread rarely allocates.
    pub fn record(&mut self, buf: Vec<f32>, count_in_beats: u32) {
        self.rec_buf = buf;
        self.rec_buf.clear();
        self.rec_start = self.playhead;
        self.count_in_total = count_in_beats as usize * self.beat_len();
        self.count_in = self.count_in_total;
        self.recording = true;
        self.playing = true;
    }

    /// True while the count-in clicks are playing.
    pub fn counting_in(&self) -> bool {
        self.count_in > 0
    }

    pub fn recording_len(&self) -> usize {
        self.rec_buf.len()
    }

    pub fn recording_start(&self) -> usize {
        self.rec_start
    }

    /// Copies recorded samples from index `from` onwards into `out` (for live waveforms).
    pub fn recorded_since(&self, from: usize, out: &mut Vec<f32>) {
        if let Some(s) = self.rec_buf.get(from..) {
            out.extend_from_slice(s);
        }
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
            t.chain.reset();
        }
        self.master_chain.reset();
    }

    // ---- audio -----------------------------------------------------------

    fn beat_len(&self) -> usize {
        (self.sr * 60.0 / self.bpm.max(20.0)) as usize
    }

    fn click(&self, pos: usize) -> f32 {
        let beat = self.beat_len();
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
        let any_solo = self.tracks.iter().any(|t| t.data.solo);
        for frame in out.chunks_exact_mut(2) {
            let x = input.pop_front().unwrap_or(0.0) * self.input_gain;
            self.in_peak = self.in_peak.max(x.abs());
            if !self.history.is_empty() {
                self.history[self.hist_pos] = x;
                self.hist_pos = (self.hist_pos + 1) % self.history.len();
                self.hist_len = (self.hist_len + 1).min(self.history.len());
            }

            let mut l = 0.0;
            let mut r = 0.0;
            if self.monitor {
                let (mut ml, mut mr) = (x, x);
                self.input_chain.process(&mut ml, &mut mr);
                l += ml;
                r += mr;
            }

            let mut click = 0.0;
            if self.playing && self.count_in > 0 {
                // Count-in: clicks only, the song waits.
                click = self.click(self.count_in_total - self.count_in);
                self.count_in -= 1;
            } else if self.playing {
                if self.recording {
                    self.rec_buf.push(x);
                }
                let pos = self.playhead;
                for t in &mut self.tracks {
                    let s = t.data.sample_at(pos);
                    let s = if t.data.invert { -s } else { s };
                    // Effects keep running between clips so reverb and delay tails ring out.
                    let (mut tl, mut tr) = (s, s);
                    t.chain.process(&mut tl, &mut tr);
                    if !t.data.mute && (!any_solo || t.data.solo) {
                        let (gl, gr) = pan_gains(t.data.pan);
                        l += tl * t.data.volume * gl;
                        r += tr * t.data.volume * gr;
                    }
                }
                if self.metronome {
                    click = self.click(pos);
                }
                self.playhead += 1;
            }
            self.master_chain.process(&mut l, &mut r);
            l += click;
            r += click;

            let l = soft_clip(l * self.master);
            let r = soft_clip(r * self.master);
            self.out_peak = self.out_peak.max(l.abs()).max(r.abs());
            frame[0] = l;
            frame[1] = r;
        }
    }
}

/// Balance-style pan: centre leaves both sides at full level.
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    ((1.0 - pan).min(1.0), (1.0 + pan).min(1.0))
}

#[inline]
fn soft_clip(x: f32) -> f32 {
    if x.abs() < 0.9 {
        x
    } else {
        x.signum() * (0.9 + 0.1 * ((x.abs() - 0.9) * 10.0).tanh())
    }
}

/// How much input "Capture" can grab after the fact.
pub const CAPTURE_SECONDS: f32 = 60.0;

/// Offline mixdown of the whole song to interleaved stereo.
pub fn render_mix(tracks: &[Track], master_fx: &[FxSlot], sr: f32) -> Vec<f32> {
    let tail = (sr * 4.0) as usize;
    let len = tracks.iter().map(Track::end).max().unwrap_or(0) + tail;
    let any_solo = tracks.iter().any(|t| t.solo);
    let mut mix = vec![0.0f32; len * 2];
    for t in tracks.iter().filter(|t| !t.mute && (!any_solo || t.solo)) {
        let (gl, gr) = pan_gains(t.pan);
        let (gl, gr) = (gl * t.volume, gr * t.volume);
        let sign = if t.invert { -1.0 } else { 1.0 };
        let mut chain = Chain::from_slots(sr, &t.fx);
        // Line up effects that add delay (e.g. the acoustic sim's convolution).
        let lat = chain.latency();
        let first = t.start();
        for pos in first..len + lat {
            let s = t.sample_at(pos) * sign;
            let (mut l, mut r) = (s, s);
            chain.process(&mut l, &mut r);
            if let Some(at) = pos.checked_sub(lat).filter(|&a| a < len && a >= first) {
                mix[at * 2] += l * gl;
                mix[at * 2 + 1] += r * gr;
            }
        }
    }
    let mut master = Chain::from_slots(sr, master_fx);
    if !master.is_empty() {
        for f in mix.chunks_exact_mut(2) {
            let (a, b) = f.split_at_mut(1);
            master.process(&mut a[0], &mut b[0]);
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
    use crate::model::Doc;

    #[test]
    fn records_and_compensates_latency() {
        let mut e = Engine::new(48000.0);
        e.monitor = false;
        e.latency_ms = 10.0; // 480 samples
        e.seek(1000);
        e.record(Vec::with_capacity(4096), 0);
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
        e.record(Vec::new(), 0);
        let mut input: VecDeque<f32> = (0..1000).map(|i| i as f32).collect();
        let mut out = vec![0.0; 2000];
        e.render(&mut input, &mut out);
        let (start, take) = e.stop().unwrap();
        assert_eq!(start, 0);
        assert_eq!(take.len(), 520);
        assert_eq!(take[0], 480.0);
    }

    #[test]
    fn count_in_delays_recording() {
        let mut e = Engine::new(48000.0);
        e.monitor = false;
        e.latency_ms = 0.0;
        e.bpm = 120.0; // 24000 samples per beat
        e.record(Vec::new(), 1);
        let mut input: VecDeque<f32> = (0..30000).map(|i| i as f32).collect();
        let mut out = vec![0.0; 60000];
        e.render(&mut input, &mut out);
        assert!(!e.counting_in());
        assert_eq!(e.playhead, 6000);
        let (start, take) = e.stop().unwrap();
        assert_eq!(start, 0);
        assert_eq!(take.len(), 6000);
        assert_eq!(take[0], 24000.0, "count-in audio isn't recorded");
    }

    #[test]
    fn capture_returns_recent_input_in_order() {
        let mut e = Engine::new(1000.0); // 60 s history = 60000 samples
        e.monitor = false;
        let mut input: VecDeque<f32> = (0..70000).map(|i| i as f32).collect();
        let mut out = vec![0.0; 140000];
        e.render(&mut input, &mut out);
        let c = e.captured(2.0);
        assert_eq!(c.len(), 2000);
        assert_eq!(c[0], 68000.0);
        assert_eq!(*c.last().unwrap(), 69999.0);
        assert_eq!(e.captured(100.0).len(), 60000);
    }

    #[test]
    fn plays_back_clips() {
        let mut e = Engine::new(48000.0);
        e.monitor = false;
        e.master = 1.0;
        let mut d = Doc::new();
        let clip = d.make_clip(vec![0.5f32; 100], 10);
        let tid = d.add_track("t".into(), Some(clip), vec![]);
        d.track_mut(tid).unwrap().volume = 1.0;
        e.set_tracks(&d.tracks);
        e.play();
        let mut out = vec![0.0; 400];
        e.render(&mut VecDeque::new(), &mut out);
        assert_eq!(out[0], 0.0);
        assert!((out[20] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn mix_follows_splits_and_trims() {
        let mut d = Doc::new();
        let clip = d.make_clip(vec![0.25f32; 4800], 0);
        let id = clip.id;
        d.add_track("t".into(), Some(clip), vec![]);
        d.trim_end(id, 2400);
        let mix = render_mix(&d.tracks, &[], 48000.0);
        assert_eq!(mix.len(), 2400 * 2);
    }
}
