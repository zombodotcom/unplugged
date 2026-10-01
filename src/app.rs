use crate::audio::{self, DeviceInfo, ErrorSlot, InputQueue, RunningAudio};
use crate::engine::{CAPTURE_SECONDS, Engine, render_mix};
use crate::fx::{FxKind, FxSlot};
use crate::model::{Doc, FxTarget, PEAK_BUCKET, Selection, Song};
use crate::project;
use crate::share::{self, PRESETS, ShareOptions};
use cpal::HostId;
use egui::{
    Align2, Color32, CursorIcon, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, UiBuilder, pos2,
    vec2,
};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const ROW_H: f32 = 76.0;
const RULER_H: f32 = 24.0;
const CTRL_W: f32 = 300.0;
/// Grab zone at clip edges for trimming, in pixels.
const EDGE_PX: f32 = 7.0;

const AMBER: Color32 = Color32::from_rgb(232, 170, 80);
const REC_RED: Color32 = Color32::from_rgb(220, 60, 60);
const SELECT: Color32 = Color32::from_rgb(255, 255, 255);
/// One colour per track, in order.
const TRACK_COLOURS: [Color32; 6] = [
    Color32::from_rgb(110, 160, 230),
    Color32::from_rgb(232, 170, 80),
    Color32::from_rgb(120, 200, 140),
    Color32::from_rgb(190, 140, 230),
    Color32::from_rgb(230, 120, 150),
    Color32::from_rgb(90, 200, 210),
];

/// State of the Share window.
#[derive(Default)]
struct ShareUi {
    open: bool,
    preset: usize,
    opts: ShareOptions,
    clip: bool,
    from: f32,
    to: f32,
    ffmpeg: Option<PathBuf>,
    /// `Some(None)` while exporting, `Some(Some(result))` when finished.
    job: Arc<Mutex<Option<Option<Result<String, String>>>>>,
    result: Option<String>,
}

/// Background autosave (crash protection). Only new audio is written each time.
struct Autosave {
    last: Instant,
    saved_rev: u64,
    busy: Arc<Mutex<bool>>,
    written: Arc<Mutex<project::WrittenAudio>>,
    /// Shown at startup when the last session didn't exit cleanly.
    offer_recovery: bool,
}

impl Default for Autosave {
    fn default() -> Self {
        Self {
            last: Instant::now(),
            saved_rev: 0,
            busy: Default::default(),
            written: Arc::new(Mutex::new(project::WrittenAudio::with_prefix(format!(
                "auto{}-",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
            )))),
            offer_recovery: false,
        }
    }
}

const AUTOSAVE_EVERY: Duration = Duration::from_secs(30);

/// Engine settings the UI edits directly.
#[derive(Clone, PartialEq)]
struct Knobs {
    monitor: bool,
    input_gain_db: f32,
    master: f32,
    metronome: bool,
    bpm: f32,
    click_volume: f32,
    latency_ms: f32,
}

/// Everything the UI needs from the engine for one frame, copied out under a short lock.
struct Snapshot {
    playing: bool,
    recording: bool,
    counting_in: bool,
    playhead: usize,
    rec_start: usize,
    rec_len: usize,
    sr: f32,
    knobs: Knobs,
    in_peak: f32,
    out_peak: f32,
}

/// Waveform of the take being recorded, built up as samples arrive.
#[derive(Default)]
struct LiveRec {
    peaks: Vec<[f32; 2]>,
    consumed: usize,
    acc: [f32; 2],
    acc_n: usize,
    scratch: Vec<f32>,
}

impl LiveRec {
    fn clear(&mut self) {
        *self = Self {
            scratch: std::mem::take(&mut self.scratch),
            ..Default::default()
        };
    }

    fn absorb(&mut self) {
        for &s in &self.scratch {
            self.acc = [self.acc[0].min(s), self.acc[1].max(s)];
            self.acc_n += 1;
            if self.acc_n == PEAK_BUCKET {
                self.peaks.push(self.acc);
                self.acc = [0.0; 2];
                self.acc_n = 0;
            }
        }
        self.consumed += self.scratch.len();
        self.scratch.clear();
    }
}

#[derive(Clone, Copy)]
enum Drag {
    /// Moving a clip. `grab` is where inside the clip it was grabbed (samples).
    Move {
        clip: u64,
        grab: usize,
        saved: bool,
    },
    TrimStart {
        clip: u64,
        saved: bool,
    },
    TrimEnd {
        clip: u64,
        saved: bool,
    },
    Scrub,
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Ruler,
    Empty { track: Option<u64> },
    ClipBody { clip: u64, track: u64 },
    ClipStart { clip: u64, track: u64 },
    ClipEnd { clip: u64, track: u64 },
}

enum Action {
    PlayPause,
    Record,
    Rewind,
    GoToEnd,
    Seek(usize),
}

pub struct App {
    engine: Arc<Mutex<Engine>>,
    queue: InputQueue,
    errors: ErrorSlot,

    hosts: Vec<HostId>,
    host: HostId,
    inputs: Vec<DeviceInfo>,
    outputs: Vec<DeviceInfo>,
    in_idx: usize,
    out_idx: usize,
    in_channel: u16,
    buffer: Option<u32>,
    audio: Option<RunningAudio>,
    show_audio: bool,

    doc: Doc,
    synced_rev: u64,
    sel: Option<Selection>,
    drag: Option<Drag>,
    menu_target: Option<Selection>,
    live: LiveRec,
    snap_to_beat: bool,
    count_in: bool,
    show_help: bool,
    autosave: Autosave,
    /// Song revision at the last manual save (to warn about unsaved work on close).
    saved_rev: u64,
    confirm_close: bool,
    allow_close: bool,

    fx_target: FxTarget,
    fx_last_sel: Option<Selection>,
    status: String,
    px_per_sec: f32,
    follow: bool,
    scroll_x: f32,
    view_w: f32,
    in_meter: f32,
    out_meter: f32,
    project_dir: Option<PathBuf>,
    take_counter: usize,
    share: ShareUi,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::new_headless(&cc.egui_ctx);
        app.autosave.offer_recovery = project::has_crash_backup();
        app.refresh_devices();
        app.start_audio(&cc.egui_ctx);
        app
    }

    /// The app without any audio devices (used by UI tests).
    pub fn new_headless(ctx: &egui::Context) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.selection.bg_fill = Color32::from_rgb(150, 100, 40);
        visuals.hyperlink_color = AMBER;
        ctx.set_visuals(visuals);

        let saved = audio::SavedDevices::load();
        let host = saved.host().unwrap_or_else(|| cpal::default_host().id());
        Self {
            engine: Arc::new(Mutex::new(Engine::new(48000.0))),
            queue: Default::default(),
            errors: Default::default(),
            hosts: audio::hosts(),
            host,
            inputs: vec![],
            outputs: vec![],
            in_idx: 0,
            out_idx: 0,
            in_channel: 0,
            buffer: saved.buffer,
            audio: None,
            show_audio: false,
            doc: Doc::new(),
            synced_rev: 0,
            sel: None,
            drag: None,
            menu_target: None,
            live: LiveRec::default(),
            snap_to_beat: false,
            count_in: false,
            show_help: false,
            autosave: Autosave::default(),
            saved_rev: 0,
            confirm_close: false,
            allow_close: false,
            fx_target: FxTarget::Input,
            fx_last_sel: None,
            status: String::new(),
            px_per_sec: 60.0,
            follow: true,
            scroll_x: 0.0,
            view_w: 800.0,
            in_meter: 0.0,
            out_meter: 0.0,
            project_dir: None,
            take_counter: 0,
            share: ShareUi::default(),
        }
    }

    /// Fills the song with generated audio (used by UI tests and screenshots).
    pub fn load_demo(&mut self) {
        let sr = 48000.0;
        let wave = |secs: f32, f: f32, amp: f32| -> Vec<f32> {
            (0..(secs * sr) as usize)
                .map(|i| {
                    let t = i as f32 / sr;
                    let env = (-(t % 1.5) * 2.5).exp();
                    (2.0 * std::f32::consts::PI * f * t).sin() * env * amp
                })
                .collect()
        };
        let c1 = self.doc.make_clip(wave(12.0, 110.0, 0.6), 0);
        let rhythm = self.doc.add_track("Rhythm".into(), Some(c1), vec![]);
        self.doc.add_fx(FxTarget::Track(rhythm), FxKind::Eq);
        self.doc.add_fx(FxTarget::Track(rhythm), FxKind::Reverb);
        let c2 = self
            .doc
            .make_clip(wave(6.0, 220.0, 0.4), (3.0 * sr) as usize);
        let c2_id = c2.id;
        let lead = self.doc.add_track("Lead".into(), Some(c2), vec![]);
        self.doc.add_fx(FxTarget::Track(lead), FxKind::Delay);
        self.doc.add_fx(FxTarget::Master, FxKind::Limiter);
        self.doc.split(c2_id, (6.0 * sr) as usize);
        if let Some(t) = self.doc.track_mut(lead) {
            t.pan = 0.4;
        }
        self.sel = Some(Selection::Clip(c2_id));
        self.take_counter = 2;
    }

    pub fn doc(&self) -> &Doc {
        &self.doc
    }

    pub fn selection(&self) -> Option<Selection> {
        self.sel
    }

    pub fn engine(&self) -> Arc<Mutex<Engine>> {
        self.engine.clone()
    }

    fn refresh_devices(&mut self) {
        let (inputs, outputs) = audio::list_devices(self.host);
        let (def_in, def_out) = audio::default_indices(self.host, &inputs, &outputs);
        self.in_idx = audio::pick_scarlett(&inputs).unwrap_or(def_in);
        self.out_idx = audio::pick_scarlett(&outputs).unwrap_or(def_out);
        self.in_channel = inputs
            .get(self.in_idx)
            .map_or(0, |d| audio::default_input_channel(&d.name));
        // Prefer whatever the user picked last time, if those devices are still around.
        let saved = audio::SavedDevices::load();
        if saved.host() == Some(self.host) {
            if let Some(i) = saved
                .input
                .and_then(|n| inputs.iter().position(|d| d.name == n))
            {
                self.in_idx = i;
                self.in_channel = saved
                    .channel
                    .filter(|&c| c < inputs[i].channels)
                    .unwrap_or(self.in_channel);
            }
            if let Some(o) = saved
                .output
                .and_then(|n| outputs.iter().position(|d| d.name == n))
            {
                self.out_idx = o;
            }
        }
        self.inputs = inputs;
        self.outputs = outputs;
    }

    fn start_audio(&mut self, ctx: &egui::Context) {
        self.audio = None; // drop old streams first
        let (Some(input), Some(output)) =
            (self.inputs.get(self.in_idx), self.outputs.get(self.out_idx))
        else {
            self.status = "No audio devices found. Is the Scarlett plugged in?".into();
            self.show_audio = true;
            return;
        };
        let mut settings = audio::AudioSettings {
            host: self.host,
            input: input.device.clone(),
            output: output.device.clone(),
            input_channel: self.in_channel,
            buffer: self.buffer,
        };
        let ctx2 = ctx.clone();
        let repaint = move || ctx2.request_repaint();
        let mut result = audio::start(
            &settings,
            self.engine.clone(),
            self.queue.clone(),
            self.errors.clone(),
            repaint.clone(),
        );
        if result.is_err() && settings.buffer.is_some() {
            // Some drivers refuse fixed buffer sizes; fall back to their default.
            settings.buffer = None;
            result = audio::start(
                &settings,
                self.engine.clone(),
                self.queue.clone(),
                self.errors.clone(),
                repaint,
            );
        }
        match result {
            Ok(a) => {
                audio::SavedDevices {
                    host: Some(self.host.name().to_string()),
                    input: Some(input.name.clone()),
                    output: Some(output.name.clone()),
                    channel: Some(self.in_channel),
                    buffer: self.buffer,
                }
                .save();
                let is_scarlett = audio::pick_scarlett(std::slice::from_ref(input)).is_some();
                let warn = if is_scarlett {
                    ""
                } else {
                    "Scarlett not found (plug it in, then Audio settings > Rescan). "
                };
                self.status = warn.to_string()
                    + &format!(
                        "Audio running: {} (input {}) -> {}",
                        input.name,
                        self.in_channel + 1,
                        output.name
                    );
                self.audio = Some(a);
            }
            Err(e) => {
                self.status = format!("Audio failed: {e:#}");
                self.show_audio = true;
            }
        }
    }

    fn snapshot(&mut self) -> Snapshot {
        let mut e = self.engine.lock();
        if e.recording {
            e.recorded_since(self.live.consumed, &mut self.live.scratch);
        }
        let s = Snapshot {
            playing: e.playing,
            recording: e.recording,
            counting_in: e.counting_in(),
            playhead: e.playhead,
            rec_start: e.recording_start(),
            rec_len: e.recording_len(),
            sr: e.sr,
            knobs: Knobs {
                monitor: e.monitor,
                input_gain_db: 20.0 * e.input_gain.max(1e-6).log10(),
                master: e.master,
                metronome: e.metronome,
                bpm: e.bpm,
                click_volume: e.click_volume,
                latency_ms: e.latency_ms,
            },
            in_peak: e.in_peak,
            out_peak: e.out_peak,
        };
        e.in_peak = 0.0;
        e.out_peak = 0.0;
        drop(e);
        self.live.absorb();
        s
    }

    fn apply_knobs(&self, k: &Knobs) {
        let mut e = self.engine.lock();
        e.monitor = k.monitor;
        e.input_gain = 10f32.powf(k.input_gain_db / 20.0);
        e.master = k.master;
        e.metronome = k.metronome;
        e.bpm = k.bpm;
        e.click_volume = k.click_volume;
        e.latency_ms = k.latency_ms;
    }

    fn handle(&mut self, a: Action, snap: &Snapshot) {
        match a {
            Action::PlayPause => {
                if snap.playing {
                    self.stop();
                } else {
                    self.engine.lock().play();
                }
            }
            Action::Record => {
                if snap.recording {
                    self.stop();
                } else {
                    if self.audio.is_none() {
                        self.status = "Start the audio device first (Audio settings).".into();
                        return;
                    }
                    // Five minutes pre-allocated so the audio thread doesn't have to grow the buffer.
                    let buf = Vec::with_capacity(snap.sr as usize * 300);
                    self.live.clear();
                    let beats = if self.count_in { 4 } else { 0 };
                    self.engine.lock().record(buf, beats);
                }
            }
            Action::Rewind => self.engine.lock().seek(0),
            Action::GoToEnd => self.engine.lock().seek(self.doc.end()),
            Action::Seek(p) => self.engine.lock().seek(p),
        }
    }

    fn stop(&mut self) {
        let take = self.engine.lock().stop();
        self.live.clear();
        if let Some((start, samples)) = take {
            self.take_counter += 1;
            let clip = self.doc.make_clip(samples, start);
            let clip_id = clip.id;
            // What you heard while recording is what you get on playback.
            let fx = self.input_fx_copy();
            self.doc
                .add_track(format!("Take {}", self.take_counter), Some(clip), fx);
            self.sel = Some(Selection::Clip(clip_id));
        }
    }

    /// A copy of the input effects for a new take (fresh ids).
    fn input_fx_copy(&mut self) -> Vec<FxSlot> {
        let fx = self.doc.input_fx.clone();
        self.doc.copy_fx(&fx)
    }

    /// "Capture": turn the last minute of playing into a take, even if you never hit record.
    fn capture(&mut self) {
        let (audio, sr, playing, playhead, lat) = {
            let e = self.engine.lock();
            (
                e.captured(CAPTURE_SECONDS),
                e.sr,
                e.playing && !e.recording,
                e.playhead,
                (e.latency_ms / 1000.0 * e.sr) as usize,
            )
        };
        // Where the first captured sample belongs on the timeline. While playing, the
        // audio lines up with the song; otherwise it goes at the playhead.
        let (mut start, mut skip) = if playing {
            let end = playhead.saturating_sub(lat);
            (end as isize - audio.len() as isize, 0usize)
        } else {
            (playhead as isize, 0)
        };
        if start < 0 {
            skip = (-start) as usize;
            start = 0;
        }
        // Trim silence at both ends (keep a little air).
        let thresh = 10f32.powf(-50.0 / 20.0);
        let pad = (0.2 * sr) as usize;
        let first = audio.iter().position(|s| s.abs() > thresh);
        let last = audio.iter().rposition(|s| s.abs() > thresh);
        let (Some(first), Some(last)) = (first, last) else {
            self.status = "Nothing to capture: no guitar heard in the last minute.".into();
            return;
        };
        let a = first.saturating_sub(pad).max(skip);
        let b = (last + pad).min(audio.len());
        if b <= a {
            self.status = "Nothing to capture.".into();
            return;
        }
        let samples = audio[a..b].to_vec();
        // Playing: keep it lined up with the song. Stopped: drop it at the playhead.
        let start = if playing {
            start as usize + (a - skip)
        } else {
            playhead
        };
        self.take_counter += 1;
        let clip = self.doc.make_clip(samples, start);
        let id = clip.id;
        let fx = self.input_fx_copy();
        self.doc
            .add_track(format!("Capture {}", self.take_counter), Some(clip), fx);
        self.sel = Some(Selection::Clip(id));
        self.status = format!(
            "Captured {:.0}s of what you just played (Ctrl+Z to undo)",
            (b - a) as f32 / sr
        );
    }

    fn tick_autosave(&mut self, snap: &Snapshot) {
        if self.autosave.offer_recovery
            || snap.recording
            || self.doc.revision == self.autosave.saved_rev
            || self.autosave.last.elapsed() < AUTOSAVE_EVERY
            || *self.autosave.busy.lock()
        {
            return;
        }
        let Some(dir) = project::autosave_dir() else {
            return;
        };
        self.autosave.last = Instant::now();
        self.autosave.saved_rev = self.doc.revision;
        let song = self.doc.song();
        let (sr, bpm) = (snap.sr as u32, snap.knobs.bpm);
        let (busy, written) = (self.autosave.busy.clone(), self.autosave.written.clone());
        *busy.lock() = true;
        std::thread::spawn(move || {
            let r = project::save_incremental(&dir, sr, bpm, &song, &mut written.lock());
            if let Err(e) = r {
                eprintln!("autosave failed: {e:#}");
            }
            *busy.lock() = false;
        });
    }

    fn recovery_window(&mut self, ctx: &egui::Context) {
        if !self.autosave.offer_recovery {
            return;
        }
        egui::Modal::new(egui::Id::new("recover")).show(ctx, |ui| {
            ui.heading("Recover your last session?");
            ui.label("Unplugged didn't close properly last time, but your work was autosaved.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Recover").strong()).clicked() {
                    self.autosave.offer_recovery = false;
                    let dir = project::autosave_dir().unwrap();
                    let sr = self.engine.lock().sr as u32;
                    match project::load(&dir, sr, &mut self.doc) {
                        Ok((pf, song)) => {
                            self.take_counter = song.tracks.len();
                            self.doc.reset(song);
                            self.engine.lock().bpm = pf.bpm;
                            self.status = "Recovered your last session. Save it somewhere with File > Save project.".into();
                        }
                        Err(e) => self.status = format!("Couldn't recover: {e:#}"),
                    }
                }
                if ui.button("Start fresh").clicked() {
                    self.autosave.offer_recovery = false;
                    project::clear_autosave();
                }
            });
        });
    }

    fn has_unsaved_work(&self) -> bool {
        self.doc.revision != self.saved_rev && self.doc.tracks.iter().any(|t| !t.clips.is_empty())
    }

    fn close_guard(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && self.has_unsaved_work()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        if !self.confirm_close {
            return;
        }
        egui::Modal::new(egui::Id::new("confirm_close")).show(ctx, |ui| {
            ui.heading("Save your song before closing?");
            ui.label("You have changes that aren't saved to a project yet.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Save").strong()).clicked() {
                    self.confirm_close = false;
                    self.save_project(false);
                    if !self.has_unsaved_work() {
                        self.allow_close = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                if ui.button("Don't save").clicked() {
                    self.confirm_close = false;
                    self.allow_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_close = false;
                }
            });
        });
    }

    fn save_chain_preset(&mut self) {
        let chain = self
            .doc
            .fx_chain(self.fx_target)
            .cloned()
            .unwrap_or_default();
        let Some(p) = rfd::FileDialog::new()
            .add_filter("Unplugged effect chain", &["json"])
            .set_file_name("my-chain.json")
            .save_file()
        else {
            return;
        };
        self.status = match serde_json::to_string_pretty(&chain).map(|j| std::fs::write(&p, j)) {
            Ok(Ok(())) => format!("Saved effect chain {}", p.display()),
            Ok(Err(e)) => format!("Couldn't save: {e}"),
            Err(e) => format!("Couldn't save: {e}"),
        };
    }

    fn load_chain_preset(&mut self) {
        let Some(p) = rfd::FileDialog::new()
            .add_filter("Unplugged effect chain", &["json"])
            .pick_file()
        else {
            return;
        };
        match std::fs::read_to_string(&p)
            .map_err(anyhow::Error::from)
            .and_then(|t| Ok(serde_json::from_str::<Vec<FxSlot>>(&t)?))
        {
            Ok(chain) => {
                self.doc.checkpoint();
                let chain = self.doc.copy_fx(&chain);
                if let Some(c) = self.doc.fx_chain_mut(self.fx_target) {
                    *c = chain;
                }
                self.doc.touch();
                self.status = format!("Loaded effect chain {}", p.display());
            }
            Err(e) => self.status = format!("Couldn't load effect chain: {e:#}"),
        }
    }

    // ---- editing ------------------------------------------------------------

    fn delete_selection(&mut self) {
        if let Some(sel) = self.sel.take()
            && self.doc.delete(sel)
        {
            self.status = match sel {
                Selection::Clip(_) => "Deleted clip (Ctrl+Z to undo)".into(),
                Selection::Track(_) => "Deleted track (Ctrl+Z to undo)".into(),
            };
        }
    }

    fn duplicate_selection(&mut self) {
        if let Some(sel) = self.sel
            && let Some(new) = self.doc.duplicate(sel)
        {
            self.sel = Some(new);
            self.status = "Duplicated (Ctrl+Z to undo)".into();
        }
    }

    fn split_at(&mut self, at: usize) {
        let n = match self.sel {
            Some(Selection::Clip(id)) => match self.doc.split(id, at) {
                Some(right) => {
                    self.sel = Some(Selection::Clip(right));
                    1
                }
                None => self.doc.split_all_at(at, None),
            },
            Some(Selection::Track(tid)) => self.doc.split_all_at(at, Some(tid)),
            None => self.doc.split_all_at(at, None),
        };
        self.status = match n {
            0 => "Nothing to split under the playhead.".into(),
            1 => "Split 1 clip at the playhead (Ctrl+Z to undo)".into(),
            n => format!("Split {n} clips at the playhead (Ctrl+Z to undo)"),
        };
    }

    fn nudge(&mut self, samples: isize) {
        if let Some(Selection::Clip(id)) = self.sel
            && let Some(c) = self.doc.clip(id)
        {
            let start = (c.start as isize + samples).max(0) as usize;
            self.doc.checkpoint();
            self.doc.move_clip(id, start, None);
        }
    }

    fn undo(&mut self) {
        self.status = if self.doc.undo() {
            "Undo".into()
        } else {
            "Nothing to undo".into()
        };
        self.drop_stale_selection();
    }

    fn redo(&mut self) {
        self.status = if self.doc.redo() {
            "Redo".into()
        } else {
            "Nothing to redo".into()
        };
        self.drop_stale_selection();
    }

    fn drop_stale_selection(&mut self) {
        let exists = match self.sel {
            Some(Selection::Clip(id)) => self.doc.find_clip(id).is_some(),
            Some(Selection::Track(id)) => self.doc.track(id).is_some(),
            None => true,
        };
        if !exists {
            self.sel = None;
        }
    }

    fn snap(&self, pos: usize, sr: f32, bpm: f32, bypass: bool) -> usize {
        if !self.snap_to_beat || bypass {
            return pos;
        }
        let beat = (sr * 60.0 / bpm.max(20.0)) as f64;
        ((pos as f64 / beat).round() * beat) as usize
    }

    fn keyboard(&mut self, ctx: &egui::Context, snap: &Snapshot, actions: &mut Vec<Action>) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (keys, cmd, shift) = ctx.input(|i| {
            let pressed = |k| i.key_pressed(k);
            (
                [
                    Key::Home,
                    Key::End,
                    Key::Space,
                    Key::R,
                    Key::C,
                    Key::Delete,
                    Key::Backspace,
                    Key::S,
                    Key::D,
                    Key::Z,
                    Key::Y,
                    Key::ArrowLeft,
                    Key::ArrowRight,
                    Key::Escape,
                    Key::Equals,
                    Key::Plus,
                    Key::Minus,
                    Key::F1,
                ]
                .map(|k| (k, pressed(k))),
                i.modifiers.command,
                i.modifiers.shift,
            )
        });
        let ms = (snap.sr / 1000.0) as isize;
        for (k, down) in keys {
            if !down {
                continue;
            }
            match (k, cmd) {
                (Key::Home, _) => actions.push(Action::Rewind),
                (Key::End, _) => actions.push(Action::GoToEnd),
                (Key::Space, _) => actions.push(Action::PlayPause),
                (Key::R, false) => actions.push(Action::Record),
                (Key::C, false) => self.capture(),
                (Key::Delete | Key::Backspace, _) => self.delete_selection(),
                (Key::S, false) => self.split_at(snap.playhead),
                (Key::S, true) => self.save_project(false),
                (Key::D, true) => self.duplicate_selection(),
                (Key::Z, true) if shift => self.redo(),
                (Key::Z, true) => self.undo(),
                (Key::Y, true) => self.redo(),
                (Key::ArrowLeft, _) => self.nudge(if shift { -ms } else { -10 * ms }),
                (Key::ArrowRight, _) => self.nudge(if shift { ms } else { 10 * ms }),
                (Key::Escape, _) => self.sel = None,
                (Key::Equals | Key::Plus, _) => {
                    self.px_per_sec = (self.px_per_sec * 1.25).min(600.0)
                }
                (Key::Minus, _) => self.px_per_sec = (self.px_per_sec / 1.25).max(10.0),
                (Key::F1, _) => self.show_help = !self.show_help,
                _ => {}
            }
        }
    }

    // ---- file operations --------------------------------------------------

    fn new_project(&mut self) {
        self.stop();
        self.doc.reset(Song::default());
        self.engine.lock().seek(0);
        self.sel = None;
        self.project_dir = None;
        self.take_counter = 0;
        self.saved_rev = self.doc.revision;
        self.status = "New project".into();
    }

    fn save_project(&mut self, save_as: bool) {
        if save_as || self.project_dir.is_none() {
            let Some(dir) = rfd::FileDialog::new()
                .set_title("Choose a folder for this project")
                .pick_folder()
            else {
                return;
            };
            self.project_dir = Some(dir);
        }
        let dir = self.project_dir.clone().unwrap();
        let (sr, bpm) = {
            let e = self.engine.lock();
            (e.sr as u32, e.bpm)
        };
        self.status = match project::save(&dir, sr, bpm, &self.doc.song()) {
            Ok(()) => {
                self.saved_rev = self.doc.revision;
                format!("Saved to {}", dir.display())
            }
            Err(e) => format!("Save failed: {e:#}"),
        };
    }

    fn open_project(&mut self) {
        let Some(dir) = rfd::FileDialog::new()
            .set_title("Open a project folder")
            .pick_folder()
        else {
            return;
        };
        let sr = self.engine.lock().sr as u32;
        match project::load(&dir, sr, &mut self.doc) {
            Ok((pf, song)) => {
                self.new_project();
                self.take_counter = song.tracks.len();
                self.doc.reset(song);
                self.engine.lock().bpm = pf.bpm;
                self.status = format!("Opened {}", dir.display());
                self.project_dir = Some(dir);
                self.saved_rev = self.doc.revision;
            }
            Err(e) => self.status = format!("Open failed: {e:#}"),
        }
    }

    fn import_wav(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("WAV audio", &["wav"])
            .pick_file()
        else {
            return;
        };
        let (sr, at) = {
            let e = self.engine.lock();
            (e.sr as u32, e.playhead)
        };
        match project::read_wav_mono(&path, sr) {
            Ok(samples) => {
                let name = path
                    .file_stem()
                    .map_or("Import".into(), |s| s.to_string_lossy().to_string());
                let clip = self.doc.make_clip(samples, at);
                self.doc.add_track(name, Some(clip), vec![]);
                self.status = format!("Imported {}", path.display());
            }
            Err(e) => self.status = format!("Import failed: {e:#}"),
        }
    }

    fn open_share(&mut self) {
        self.share.open = true;
        self.share.ffmpeg = share::find_ffmpeg();
    }

    fn start_share(&mut self) {
        let preset = &PRESETS[self.share.preset];
        let base = match self.share.opts.title.trim() {
            "" => "song".to_string(),
            t => t.replace(|c: char| !c.is_alphanumeric() && c != ' ' && c != '-', ""),
        };
        let Some(out) = rfd::FileDialog::new()
            .add_filter(preset.name, &[preset.extension()])
            .set_file_name(format!("{base}-{}.{}", preset.id, preset.extension()))
            .save_file()
        else {
            return;
        };
        let sr = self.engine.lock().sr;
        let tracks = self.doc.tracks.clone();
        let master_fx = self.doc.master_fx.clone();
        if tracks.iter().all(|t| t.clips.is_empty()) {
            self.share.result = Some("Record something first.".into());
            return;
        }
        let opts = self.share.opts.clone();
        let clip = self.share.clip.then_some((self.share.from, self.share.to));
        let job = self.share.job.clone();
        *job.lock() = Some(None);
        self.share.result = None;
        std::thread::spawn(move || {
            let run = || -> anyhow::Result<String> {
                let mut mix = render_mix(&tracks, &master_fx, sr);
                if let Some((from, to)) = clip {
                    let frames = mix.len() / 2;
                    let a = ((from * sr) as usize).min(frames);
                    let b = ((to * sr) as usize).min(frames);
                    anyhow::ensure!(b > a, "The clip end must be after its start.");
                    mix = mix[a * 2..b * 2].to_vec();
                    share::fade_edges(&mut mix, sr as u32, 0.02, 0.5);
                }
                Ok(share::export(&mix, sr as u32, preset, &opts, &out)?.summary())
            };
            *job.lock() = Some(Some(run().map_err(|e| format!("Export failed: {e:#}"))));
        });
    }

    fn share_window(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        // Pick up a finished export.
        let finished = {
            let mut job = self.share.job.lock();
            match job.take() {
                Some(Some(r)) => Some(r),
                other => {
                    *job = other;
                    None
                }
            }
        };
        if let Some(r) = finished {
            self.share.result = Some(match r {
                Ok(s) | Err(s) => s,
            });
        }
        let busy = self.share.job.lock().is_some();

        let mut open = self.share.open;
        egui::Window::new("Share / Export")
            .open(&mut open)
            .resizable(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                let ffmpeg = self.share.ffmpeg.is_some();
                ui.label(RichText::new("Where's it going?").strong());
                for (i, p) in PRESETS.iter().enumerate() {
                    ui.add_enabled_ui(ffmpeg || !p.needs_ffmpeg(), |ui| {
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.share.preset, i, p.name);
                            ui.label(RichText::new(p.hint).small().weak());
                        });
                    });
                }
                if !ffmpeg {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(share::FFMPEG_HELP).small().color(AMBER));
                        if ui.small_button("Check again").clicked() {
                            self.share.ffmpeg = share::find_ffmpeg();
                        }
                    });
                }
                let preset = &PRESETS[self.share.preset];

                ui.separator();
                egui::Grid::new("share_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Title");
                    ui.text_edit_singleline(&mut self.share.opts.title);
                    ui.end_row();
                    ui.label("Artist");
                    ui.text_edit_singleline(&mut self.share.opts.artist);
                    ui.end_row();
                    if preset.needs_ffmpeg() {
                        ui.label("Cover picture");
                        ui.horizontal(|ui| {
                            let name = self.share.opts.cover.as_ref().and_then(|p| p.file_name()).map_or(
                                "none (plain background)".to_string(),
                                |n| n.to_string_lossy().to_string(),
                            );
                            ui.label(name);
                            if ui.small_button("Pick…").clicked()
                                && let Some(p) = rfd::FileDialog::new()
                                    .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp"])
                                    .pick_file()
                            {
                                self.share.opts.cover = Some(p);
                            }
                            if self.share.opts.cover.is_some() && ui.small_button("✖").clicked() {
                                self.share.opts.cover = None;
                            }
                        });
                        ui.end_row();
                    }
                    ui.label("Loudness");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.share.opts.normalize, "Match to").on_hover_text(
                            "Most platforms play everything at about -14 LUFS. Matching it means yours isn't turned down or squashed.",
                        );
                        ui.add_enabled(
                            self.share.opts.normalize,
                            egui::DragValue::new(&mut self.share.opts.target_lufs)
                                .range(-24.0..=-8.0)
                                .speed(0.1)
                                .suffix(" LUFS"),
                        );
                    });
                    ui.end_row();
                    ui.label("What");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.share.clip, false, "Whole song");
                        ui.radio_value(&mut self.share.clip, true, "Clip");
                    });
                    ui.end_row();
                    if self.share.clip {
                        let now = snap.playhead as f32 / snap.sr;
                        ui.label("");
                        ui.horizontal(|ui| {
                            ui.add(egui::DragValue::new(&mut self.share.from).range(0.0..=36000.0).speed(0.1).suffix(" s"));
                            if ui.small_button("at playhead").on_hover_text("Start the clip at the playhead").clicked() {
                                self.share.from = now;
                            }
                            ui.label("to");
                            ui.add(egui::DragValue::new(&mut self.share.to).range(0.0..=36000.0).speed(0.1).suffix(" s"));
                            if ui.small_button("at playhead").on_hover_text("End the clip at the playhead").clicked() {
                                self.share.to = now;
                            }
                        });
                        ui.end_row();
                    }
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let can = !busy && (ffmpeg || !preset.needs_ffmpeg());
                    if ui.add_enabled(can, egui::Button::new(RichText::new("Export…").strong())).clicked() {
                        self.start_share();
                    }
                    if busy {
                        ui.spinner();
                        ui.label(if preset.needs_ffmpeg() { "Rendering video…" } else { "Exporting…" });
                    }
                });
                if let Some(r) = &self.share.result {
                    ui.add_space(4.0);
                    ui.label(RichText::new(r).small());
                }
            });
        self.share.open = open;
    }

    // ---- UI pieces ---------------------------------------------------------

    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("Keyboard shortcuts")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("keys")
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        for (k, what) in [
                            ("Space", "Play / pause"),
                            ("R", "Record a new layer / stop"),
                            (
                                "C",
                                "Capture: keep the last minute you played, even without recording",
                            ),
                            ("Home / End", "Jump to start / end"),
                            (
                                "Click a clip",
                                "Select it (click a track's header to select the track)",
                            ),
                            ("Drag a clip", "Move it (also onto another track)"),
                            ("Drag a clip's edge", "Trim it"),
                            ("Right-click", "Menu for a clip or track"),
                            ("S", "Split at the playhead"),
                            ("Delete / Backspace", "Delete the selected clip or track"),
                            ("Ctrl+D", "Duplicate"),
                            ("Ctrl+Z", "Undo"),
                            ("Ctrl+Shift+Z / Ctrl+Y", "Redo"),
                            (
                                "Left / Right arrow",
                                "Nudge selected clip 10 ms (Shift: 1 ms)",
                            ),
                            ("Alt while dragging", "Ignore snap"),
                            ("+ / − or Ctrl+wheel", "Zoom"),
                            ("Ctrl+S", "Save project"),
                            ("Esc", "Deselect"),
                            ("F1", "This list"),
                        ] {
                            ui.label(RichText::new(k).monospace().strong());
                            ui.label(what);
                            ui.end_row();
                        }
                    });
            });
        self.show_help = open;
    }

    fn audio_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_audio;
        let mut restart = false;
        let before = (
            self.host,
            self.in_idx,
            self.out_idx,
            self.in_channel,
            self.buffer,
        );
        egui::Window::new("Audio settings")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("audio_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Driver");
                        let before = self.host;
                        egui::ComboBox::from_id_salt("host")
                            .selected_text(self.host.name())
                            .show_ui(ui, |ui| {
                                for h in &self.hosts {
                                    ui.selectable_value(&mut self.host, *h, h.name());
                                }
                            });
                        if before != self.host {
                            self.refresh_devices();
                        }
                        ui.end_row();

                        ui.label("Input (guitar)");
                        let before = self.in_idx;
                        let name = self
                            .inputs
                            .get(self.in_idx)
                            .map_or("—".into(), |d| d.name.clone());
                        egui::ComboBox::from_id_salt("in")
                            .width(300.0)
                            .selected_text(name)
                            .show_ui(ui, |ui| {
                                for (i, d) in self.inputs.iter().enumerate() {
                                    ui.selectable_value(&mut self.in_idx, i, &d.name);
                                }
                            });
                        if before != self.in_idx {
                            self.in_channel = self
                                .inputs
                                .get(self.in_idx)
                                .map_or(0, |d| audio::default_input_channel(&d.name));
                        }
                        ui.end_row();

                        ui.label("Input channel");
                        let chans = self.inputs.get(self.in_idx).map_or(1, |d| d.channels);
                        egui::ComboBox::from_id_salt("ch")
                            .selected_text(format!("Input {}", self.in_channel + 1))
                            .show_ui(ui, |ui| {
                                for c in 0..chans {
                                    ui.selectable_value(
                                        &mut self.in_channel,
                                        c,
                                        format!("Input {}", c + 1),
                                    );
                                }
                            });
                        ui.end_row();

                        ui.label("Output (headphones / speakers)");
                        let name = self
                            .outputs
                            .get(self.out_idx)
                            .map_or("—".into(), |d| d.name.clone());
                        egui::ComboBox::from_id_salt("out")
                            .width(300.0)
                            .selected_text(name)
                            .show_ui(ui, |ui| {
                                for (i, d) in self.outputs.iter().enumerate() {
                                    ui.selectable_value(&mut self.out_idx, i, &d.name);
                                }
                            });
                        ui.end_row();

                        ui.label("Buffer size");
                        let label = |b: Option<u32>| {
                            b.map_or("Driver default".to_string(), |b| format!("{b} frames"))
                        };
                        egui::ComboBox::from_id_salt("buf")
                            .selected_text(label(self.buffer))
                            .show_ui(ui, |ui| {
                                for b in
                                    [None, Some(64), Some(128), Some(256), Some(512), Some(1024)]
                                {
                                    ui.selectable_value(&mut self.buffer, b, label(b));
                                }
                            });
                        ui.end_row();
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Restart audio").clicked() {
                        restart = true;
                    }
                    if ui.button("Rescan devices").clicked() {
                        self.refresh_devices();
                    }
                });
                if let Some(a) = &self.audio {
                    ui.label(RichText::new(&a.description).weak());
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Scarlett Solo: the guitar jack is Input 2. If the Scarlett's Direct Monitor \
                         button is on, you also hear the plain guitar; turn it off, or turn off \
                         🎧 Monitor here.",
                    )
                    .small()
                    .weak(),
                );
            });
        self.show_audio = open;
        // Any device/channel/buffer change takes effect right away.
        if before
            != (
                self.host,
                self.in_idx,
                self.out_idx,
                self.in_channel,
                self.buffer,
            )
        {
            restart = true;
        }
        if restart {
            self.stop();
            self.start_audio(ctx);
        }
    }

    fn side_panel(&mut self, ui: &mut egui::Ui, knobs: &mut Knobs) {
        ui.heading("Input");
        ui.add(egui::Slider::new(&mut knobs.input_gain_db, -12.0..=24.0).text("Gain dB"))
            .on_hover_text(
                "Software boost for the guitar signal. Set the Scarlett's own gain knob first.",
            );
        meter(ui, "In", self.in_meter);
        meter(ui, "Out", self.out_meter);
        ui.add(egui::Slider::new(&mut knobs.latency_ms, 0.0..=80.0).text("Latency ms"))
            .on_hover_text(
                "Lines new takes up with old ones. If a new take sounds late against the others, raise this; if early, lower it.",
            );

        ui.separator();
        self.fx_panel(ui);
    }

    fn fx_panel(&mut self, ui: &mut egui::Ui) {
        // Follow the selection: picking a clip or track shows that track's effects.
        let sel_track = match self.sel {
            Some(Selection::Track(t)) => Some(t),
            Some(Selection::Clip(c)) => self.doc.find_clip(c).map(|(ti, _)| self.doc.tracks[ti].id),
            None => None,
        };
        if self.sel != self.fx_last_sel {
            self.fx_last_sel = self.sel;
            if let Some(t) = sel_track {
                self.fx_target = FxTarget::Track(t);
            }
        }
        if self.doc.fx_chain(self.fx_target).is_none() {
            self.fx_target = FxTarget::Input;
        }

        ui.heading("Effects");
        ui.horizontal_wrapped(|ui| {
            let shown = match self.fx_target {
                FxTarget::Track(t) => Some(t),
                _ => sel_track,
            };
            if let Some(t) = shown.and_then(|t| self.doc.track(t)) {
                let (id, name) = (t.id, t.name.clone());
                ui.selectable_value(&mut self.fx_target, FxTarget::Track(id), name)
                    .on_hover_text("Effects on this track");
            }
            ui.selectable_value(&mut self.fx_target, FxTarget::Input, "Input").on_hover_text(
                "What you hear live while you play (with 🎧 Monitor on). New takes start with a copy of these.",
            );
            ui.selectable_value(&mut self.fx_target, FxTarget::Master, "Master")
                .on_hover_text("Effects on the whole mix");
        });
        ui.add_space(4.0);

        let target = self.fx_target;
        let Some(chain) = self.doc.fx_chain(target).cloned() else {
            return;
        };
        let mut edited = chain.clone();
        let mut undo_point = false;
        let mut remove = None;
        let mut shift: Option<(usize, isize)> = None;
        if edited.is_empty() {
            ui.label(RichText::new("No effects yet. Add one below.").weak());
        }
        ui.spacing_mut().slider_width = 110.0;
        for (i, slot) in edited.iter_mut().enumerate() {
            egui::Frame::group(ui.style())
                .inner_margin(6.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .checkbox(&mut slot.on, "")
                            .on_hover_text("On / off")
                            .changed()
                        {
                            undo_point = true;
                        }
                        let name = RichText::new(slot.kind.name()).strong();
                        ui.label(if slot.on { name } else { name.weak() })
                            .on_hover_text(slot.kind.about());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✖").on_hover_text("Remove").clicked() {
                                remove = Some(i);
                            }
                            if ui.small_button("⏷").on_hover_text("Move down").clicked() {
                                shift = Some((i, 1));
                            }
                            if ui.small_button("⏶").on_hover_text("Move up").clicked() {
                                shift = Some((i, -1));
                            }
                        });
                    });
                    ui.add_enabled_ui(slot.on, |ui| {
                        let mut vals = slot.values();
                        for (j, d) in slot.kind.params().iter().enumerate() {
                            let r = if d.switch {
                                let mut b = vals[j] > 0.5;
                                let r = ui.checkbox(&mut b, d.name);
                                vals[j] = if b { 1.0 } else { 0.0 };
                                r
                            } else {
                                let suffix = if d.unit.is_empty() {
                                    String::new()
                                } else {
                                    format!(" {}", d.unit)
                                };
                                ui.add(
                                    egui::Slider::new(&mut vals[j], d.min..=d.max)
                                        .logarithmic(d.log)
                                        .text(d.name)
                                        .suffix(suffix),
                                )
                            };
                            if r.drag_started() || (r.changed() && !r.dragged()) {
                                undo_point = true;
                            }
                            if r.double_clicked() && !d.switch {
                                vals[j] = d.default;
                                undo_point = true;
                            }
                        }
                        slot.params = vals;
                    });
                });
        }
        if let Some(i) = remove {
            edited.remove(i);
            undo_point = true;
        }
        if let Some((i, d)) = shift {
            let j = (i as isize + d).clamp(0, edited.len() as isize - 1) as usize;
            edited.swap(i, j);
            undo_point = true;
        }
        if edited != chain {
            if undo_point {
                self.doc.checkpoint();
            }
            if let Some(c) = self.doc.fx_chain_mut(target) {
                *c = edited;
            }
            self.doc.touch();
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.menu_button(RichText::new("➕ Add effect").strong(), |ui| {
                for kind in FxKind::ALL {
                    if ui.button(kind.name()).on_hover_text(kind.about()).clicked() {
                        self.doc.add_fx(target, kind);
                        ui.close();
                    }
                }
            });
            ui.menu_button("Chain…", |ui| {
                if ui.button("Save chain as preset…").clicked() {
                    self.save_chain_preset();
                    ui.close();
                }
                if ui.button("Load preset into this chain…").clicked() {
                    self.load_chain_preset();
                    ui.close();
                }
            });
        });
        ui.label(
            RichText::new("Double-click a knob to reset it.")
                .small()
                .weak(),
        );
    }

    fn transport(
        &mut self,
        ui: &mut egui::Ui,
        snap: &Snapshot,
        knobs: &mut Knobs,
        actions: &mut Vec<Action>,
    ) {
        ui.horizontal(|ui| {
            let big = |s: &str| RichText::new(s).size(18.0);
            if ui
                .button(big("⏮"))
                .on_hover_text("Back to start (Home)")
                .clicked()
            {
                actions.push(Action::Rewind);
            }
            let play_label = if snap.playing { "⏸" } else { "▶" };
            if ui
                .button(big(play_label))
                .on_hover_text("Play / pause (Space)")
                .clicked()
            {
                actions.push(Action::PlayPause);
            }
            let rec = if snap.recording {
                RichText::new("⏹ Stop").size(18.0).color(Color32::WHITE)
            } else {
                RichText::new("⏺ Rec").size(18.0).color(REC_RED)
            };
            let rec_btn = egui::Button::new(rec).fill(if snap.recording {
                REC_RED
            } else {
                ui.visuals().widgets.inactive.bg_fill
            });
            if ui
                .add(rec_btn)
                .on_hover_text("Record a new layer from the playhead (R)")
                .clicked()
            {
                actions.push(Action::Record);
            }
            if ui
                .button(RichText::new("⟲ Capture").size(15.0))
                .on_hover_text(
                    "Forgot to hit record? Turns the last minute you played into a take (C).\n\
                     If the song was playing, the take lines up with it.",
                )
                .clicked()
            {
                self.capture();
            }

            ui.add_space(8.0);
            ui.label(
                RichText::new(fmt_time(snap.playhead as f32 / snap.sr))
                    .monospace()
                    .size(20.0),
            );
            ui.add_space(8.0);
            ui.separator();
            toggle(ui, &mut knobs.monitor, "🎧 Monitor").on_hover_text(
                "Hear your guitar live through Unplugged, through the Input effects.\n\
                 Turn this off if you use the Scarlett's Direct Monitor button, or you'll hear yourself twice.",
            );
            ui.separator();
            toggle(ui, &mut knobs.metronome, "Click")
                .on_hover_text("Metronome while playing and recording");
            ui.add(
                egui::DragValue::new(&mut knobs.bpm)
                    .range(30.0..=300.0)
                    .suffix(" bpm")
                    .speed(0.5),
            );
            toggle(ui, &mut self.count_in, "Count-in")
                .on_hover_text("Play 4 clicks before recording starts");
            toggle(ui, &mut self.snap_to_beat, "Snap")
                .on_hover_text("Clips snap to beats when you drag them (hold Alt to ignore)");
            ui.separator();
            if ui
                .add_enabled(self.doc.can_undo(), egui::Button::new("Undo"))
                .on_hover_text("Ctrl+Z")
                .clicked()
            {
                self.undo();
            }
            if ui
                .add_enabled(self.doc.can_redo(), egui::Button::new("Redo"))
                .on_hover_text("Ctrl+Shift+Z")
                .clicked()
            {
                self.redo();
            }
            ui.separator();
            ui.label("Master vol");
            ui.add(egui::Slider::new(&mut knobs.master, 0.0..=1.5).show_value(false));
            ui.separator();
            toggle(ui, &mut self.follow, "Follow")
                .on_hover_text("Scroll the view along with the playhead");
            ui.add(
                egui::Slider::new(&mut self.px_per_sec, 10.0..=600.0)
                    .logarithmic(true)
                    .show_value(false),
            )
            .on_hover_text("Zoom (+ / −, or Ctrl+mouse wheel)");
        });
    }

    fn timeline(&mut self, ui: &mut egui::Ui, snap: &Snapshot, actions: &mut Vec<Action>) {
        if self.doc.tracks.is_empty() && !snap.recording {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No tracks yet").size(22.0));
                ui.label("Hit ⏺ Rec (or press R) and play. Every recording becomes a new layer.");
                ui.label("Record again to layer on top while the earlier takes play back.");
                ui.label(RichText::new("Press F1 for keyboard shortcuts.").weak());
            });
            return;
        }
        let rows = self.doc.tracks.len() + usize::from(snap.recording);
        let total_h = RULER_H + rows as f32 * ROW_H;
        let sr = snap.sr;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let (col, _) = ui.allocate_exact_size(vec2(CTRL_W, total_h), Sense::hover());
                    self.track_headers(ui, col, snap);

                    let len_samples = self
                        .doc
                        .end()
                        .max(snap.rec_start + snap.rec_len)
                        .max(snap.playhead);
                    let content_w = ((len_samples as f32 / sr + 30.0) * self.px_per_sec)
                        .max(ui.available_width());
                    let playhead_x = snap.playhead as f32 / sr * self.px_per_sec;
                    let mut area = egui::ScrollArea::horizontal()
                        .id_salt("timeline")
                        .auto_shrink([false, true]);
                    if self.follow
                        && snap.playing
                        && self.drag.is_none()
                        && (playhead_x < self.scroll_x
                            || playhead_x > self.scroll_x + self.view_w - 40.0)
                    {
                        area = area.horizontal_scroll_offset((playhead_x - 40.0).max(0.0));
                    }
                    let out = area.show(ui, |ui| {
                        let (rect, resp) = ui
                            .allocate_exact_size(vec2(content_w, total_h), Sense::click_and_drag());
                        self.timeline_input(ui, rect, &resp, snap, actions);
                        self.paint_timeline(ui, rect, snap);
                    });
                    self.scroll_x = out.state.offset.x;
                    self.view_w = out.inner_rect.width();
                });
            });
    }

    fn track_headers(&mut self, ui: &mut egui::Ui, col: Rect, snap: &Snapshot) {
        let ids: Vec<u64> = self.doc.tracks.iter().map(|t| t.id).collect();
        for (i, tid) in ids.into_iter().enumerate() {
            let r = Rect::from_min_size(
                col.min + vec2(0.0, RULER_H + i as f32 * ROW_H),
                vec2(CTRL_W, ROW_H),
            )
            .shrink2(vec2(2.0, 2.0));
            let selected = self.sel == Some(Selection::Track(tid));
            // Background: click selects the track, right-click opens its menu.
            let bg = ui.interact(r, ui.id().with(("track_bg", tid)), Sense::click());
            if bg.clicked() || bg.secondary_clicked() {
                self.sel = Some(Selection::Track(tid));
            }
            ui.painter().rect_filled(r, 4.0, row_bg(ui, i));
            if selected {
                ui.painter()
                    .rect_stroke(r, 4.0, Stroke::new(1.5, AMBER), egui::StrokeKind::Inside);
            }
            bg.context_menu(|ui| self.track_menu(ui, tid, snap));

            let mut child = ui.new_child(
                UiBuilder::new()
                    .max_rect(r.shrink(6.0))
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            self.track_controls(&mut child, tid);
        }
        if snap.recording {
            let r = Rect::from_min_size(
                col.min + vec2(0.0, RULER_H + self.doc.tracks.len() as f32 * ROW_H),
                vec2(CTRL_W, ROW_H),
            );
            let label = if snap.counting_in {
                "Count-in…"
            } else {
                "⏺ Recording…"
            };
            ui.painter().text(
                r.left_center() + vec2(12.0, 0.0),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(16.0),
                REC_RED,
            );
        }
    }

    fn track_controls(&mut self, ui: &mut egui::Ui, tid: u64) {
        let Some(t) = self.doc.track(tid) else { return };
        let mut v = t.clone();
        let mut delete = false;
        let mut toggled = false;
        let mut open_fx = false;
        ui.spacing_mut().item_spacing = vec2(4.0, 6.0);
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut v.name).desired_width(118.0));
            let mut toggle =
                |ui: &mut egui::Ui, on: &mut bool, label: &str, col: Color32, tip: &str| {
                    let text = if *on {
                        RichText::new(label).color(Color32::BLACK).strong()
                    } else {
                        RichText::new(label)
                    };
                    let b = egui::Button::new(text)
                        .min_size(vec2(24.0, 0.0))
                        .fill(if *on {
                            col
                        } else {
                            ui.visuals().widgets.inactive.bg_fill
                        });
                    if ui.add(b).on_hover_text(tip).clicked() {
                        *on = !*on;
                        toggled = true;
                    }
                };
            toggle(
                ui,
                &mut v.mute,
                "M",
                Color32::from_rgb(200, 200, 90),
                "Mute",
            );
            toggle(
                ui,
                &mut v.solo,
                "S",
                Color32::from_rgb(90, 200, 120),
                "Solo",
            );
            let active = v.fx.iter().filter(|f| f.on).count();
            let label = if active > 0 {
                format!("FX {active}")
            } else {
                "FX".to_string()
            };
            let text = if active > 0 {
                RichText::new(label).color(Color32::BLACK).strong()
            } else {
                RichText::new(label)
            };
            let fx_btn = egui::Button::new(text)
                .min_size(vec2(24.0, 0.0))
                .fill(if active > 0 {
                    AMBER
                } else {
                    ui.visuals().widgets.inactive.bg_fill
                });
            if ui
                .add(fx_btn)
                .on_hover_text("Show this track's effects")
                .clicked()
            {
                open_fx = true;
            }
            if ui
                .add(egui::Button::new("✖").min_size(vec2(24.0, 0.0)))
                .on_hover_text("Delete track (or select it and press Delete)")
                .clicked()
            {
                delete = true;
            }
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = 90.0;
            ui.label("Vol");
            let a = ui.add(egui::Slider::new(&mut v.volume, 0.0..=1.5).show_value(false));
            ui.label("Pan");
            ui.spacing_mut().slider_width = 70.0;
            let b = ui
                .add(egui::Slider::new(&mut v.pan, -1.0..=1.0).show_value(false))
                .on_hover_text(
                    "Pan layered takes left and right for a wide sound (double-click to centre)",
                );
            if b.double_clicked() {
                v.pan = 0.0;
            }
            if a.drag_started() || b.drag_started() {
                self.doc.checkpoint();
            }
        });

        if delete {
            self.sel = Some(Selection::Track(tid));
            self.delete_selection();
            return;
        }
        if open_fx {
            self.sel = Some(Selection::Track(tid));
            self.fx_last_sel = self.sel;
            self.fx_target = FxTarget::Track(tid);
        }
        let t = self.doc.track(tid).unwrap();
        let changed = v.name != t.name
            || v.volume != t.volume
            || v.pan != t.pan
            || v.mute != t.mute
            || v.solo != t.solo;
        if changed {
            if toggled {
                self.doc.checkpoint();
            }
            *self.doc.track_mut(tid).unwrap() = v;
            self.doc.touch();
        }
    }

    fn track_menu(&mut self, ui: &mut egui::Ui, tid: u64, snap: &Snapshot) {
        self.sel = Some(Selection::Track(tid));
        if ui.button("Duplicate track    Ctrl+D").clicked() {
            self.duplicate_selection();
            ui.close();
        }
        if ui.button("Split all clips at playhead    S").clicked() {
            self.split_at(snap.playhead);
            ui.close();
        }
        let mut invert = self.doc.track(tid).is_some_and(|t| t.invert);
        if ui
            .checkbox(&mut invert, "Flip polarity (Ø)")
            .on_hover_text("Fixes thin, hollow sound when two takes of the same part cancel out")
            .changed()
        {
            self.doc.checkpoint();
            if let Some(t) = self.doc.track_mut(tid) {
                t.invert = invert;
            }
            self.doc.touch();
        }
        if ui.button("Move up").clicked() {
            self.doc.move_track(tid, -1);
            ui.close();
        }
        if ui.button("Move down").clicked() {
            self.doc.move_track(tid, 1);
            ui.close();
        }
        ui.separator();
        if ui.button("Delete track    Del").clicked() {
            self.delete_selection();
            ui.close();
        }
    }

    fn clip_menu(&mut self, ui: &mut egui::Ui, clip: u64, snap: &Snapshot) {
        if ui.button("Split at playhead    S").clicked() {
            self.sel = Some(Selection::Clip(clip));
            self.split_at(snap.playhead);
            ui.close();
        }
        if ui.button("Duplicate    Ctrl+D").clicked() {
            self.sel = Some(Selection::Clip(clip));
            self.duplicate_selection();
            ui.close();
        }
        if ui.button("Move to playhead").clicked() {
            self.doc.checkpoint();
            self.doc.move_clip(clip, snap.playhead, None);
            ui.close();
        }
        ui.separator();
        if ui.button("Delete    Del").clicked() {
            self.sel = Some(Selection::Clip(clip));
            self.delete_selection();
            ui.close();
        }
    }

    fn hit_test(&self, rect: Rect, p: Pos2, sr: f32) -> Hit {
        if p.y < rect.top() + RULER_H {
            return Hit::Ruler;
        }
        let row = ((p.y - rect.top() - RULER_H) / ROW_H) as usize;
        let Some(t) = self.doc.tracks.get(row) else {
            return Hit::Empty { track: None };
        };
        let x_of = |s: usize| rect.left() + s as f32 / sr * self.px_per_sec;
        for c in t.clips.iter().rev() {
            let (x0, x1) = (x_of(c.start), x_of(c.end()));
            if p.x >= x0 - 2.0 && p.x <= x1 + 2.0 {
                let edges = x1 - x0 > EDGE_PX * 3.0;
                return if edges && p.x - x0 < EDGE_PX {
                    Hit::ClipStart {
                        clip: c.id,
                        track: t.id,
                    }
                } else if edges && x1 - p.x < EDGE_PX {
                    Hit::ClipEnd {
                        clip: c.id,
                        track: t.id,
                    }
                } else {
                    Hit::ClipBody {
                        clip: c.id,
                        track: t.id,
                    }
                };
            }
        }
        Hit::Empty { track: Some(t.id) }
    }

    fn timeline_input(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        resp: &egui::Response,
        snap: &Snapshot,
        actions: &mut Vec<Action>,
    ) {
        let sr = snap.sr;
        let pps = self.px_per_sec;
        let to_samples = |x: f32| ((x - rect.left()).max(0.0) / pps * sr) as usize;
        let alt = ui.input(|i| i.modifiers.alt);

        // Ctrl + wheel zooms.
        if resp.hovered() {
            let (ctrl, dy) = ui.input(|i| (i.modifiers.ctrl, i.smooth_scroll_delta.y));
            if ctrl && dy != 0.0 {
                self.px_per_sec = (self.px_per_sec * (1.0 + dy * 0.003)).clamp(10.0, 600.0);
            }
        }

        // Cursor feedback.
        if let Some(p) = resp.hover_pos() {
            let icon = match (self.drag, self.hit_test(rect, p, sr)) {
                (Some(Drag::Move { .. }), _) => CursorIcon::Grabbing,
                (Some(Drag::TrimStart { .. } | Drag::TrimEnd { .. }), _) => {
                    CursorIcon::ResizeHorizontal
                }
                (_, Hit::ClipStart { .. } | Hit::ClipEnd { .. }) => CursorIcon::ResizeHorizontal,
                (_, Hit::ClipBody { .. }) => CursorIcon::Grab,
                _ => CursorIcon::Default,
            };
            ui.ctx().set_cursor_icon(icon);
        }

        if resp.drag_started()
            && let Some(p) = ui.input(|i| i.pointer.press_origin())
        {
            self.drag = Some(match self.hit_test(rect, p, sr) {
                Hit::ClipBody { clip, .. } => {
                    self.sel = Some(Selection::Clip(clip));
                    let start = self.doc.clip(clip).map_or(0, |c| c.start);
                    Drag::Move {
                        clip,
                        grab: to_samples(p.x).saturating_sub(start),
                        saved: false,
                    }
                }
                Hit::ClipStart { clip, .. } => {
                    self.sel = Some(Selection::Clip(clip));
                    Drag::TrimStart { clip, saved: false }
                }
                Hit::ClipEnd { clip, .. } => {
                    self.sel = Some(Selection::Clip(clip));
                    Drag::TrimEnd { clip, saved: false }
                }
                Hit::Ruler | Hit::Empty { .. } => Drag::Scrub,
            });
        }

        if resp.dragged()
            && let (Some(drag), Some(p)) = (self.drag, resp.interact_pointer_pos())
        {
            let at = to_samples(p.x);
            let bpm = snap.knobs.bpm;
            let save = |doc: &mut Doc, saved: bool| {
                if !saved {
                    doc.checkpoint();
                }
                true
            };
            self.drag = Some(match drag {
                Drag::Scrub => {
                    actions.push(Action::Seek(at));
                    Drag::Scrub
                }
                Drag::Move { clip, grab, saved } => {
                    let start = self.snap(at.saturating_sub(grab), sr, bpm, alt);
                    let row = ((p.y - rect.top() - RULER_H) / ROW_H).max(0.0) as usize;
                    let dest = self.doc.tracks.get(row).map(|t| t.id);
                    let cur = self
                        .doc
                        .find_clip(clip)
                        .map(|(t, c)| (self.doc.tracks[t].id, self.doc.tracks[t].clips[c].start));
                    let moved =
                        cur.is_some_and(|(tid, s)| s != start || dest.is_some_and(|d| d != tid));
                    let saved = if moved {
                        save(&mut self.doc, saved)
                    } else {
                        saved
                    };
                    if moved {
                        self.doc.move_clip(clip, start, dest);
                    }
                    Drag::Move { clip, grab, saved }
                }
                Drag::TrimStart { clip, saved } => {
                    let saved = save(&mut self.doc, saved);
                    self.doc.trim_start(clip, self.snap(at, sr, bpm, alt));
                    Drag::TrimStart { clip, saved }
                }
                Drag::TrimEnd { clip, saved } => {
                    let saved = save(&mut self.doc, saved);
                    self.doc.trim_end(clip, self.snap(at, sr, bpm, alt));
                    Drag::TrimEnd { clip, saved }
                }
            });
        }
        if resp.drag_stopped() {
            self.drag = None;
        }

        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            match self.hit_test(rect, p, sr) {
                Hit::ClipBody { clip, .. }
                | Hit::ClipStart { clip, .. }
                | Hit::ClipEnd { clip, .. } => {
                    self.sel = Some(Selection::Clip(clip));
                }
                Hit::Ruler | Hit::Empty { .. } => {
                    self.sel = None;
                    actions.push(Action::Seek(to_samples(p.x)));
                }
            }
        }
        if resp.double_clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            // Double-click a clip: put the playhead there (handy before pressing S).
            actions.push(Action::Seek(to_samples(p.x)));
        }
        if resp.secondary_clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.menu_target = match self.hit_test(rect, p, sr) {
                Hit::ClipBody { clip, .. }
                | Hit::ClipStart { clip, .. }
                | Hit::ClipEnd { clip, .. } => Some(Selection::Clip(clip)),
                Hit::Empty { track: Some(t) } => Some(Selection::Track(t)),
                _ => None,
            };
            self.sel = self.menu_target;
        }
        resp.context_menu(|ui| match self.menu_target {
            Some(Selection::Clip(c)) => self.clip_menu(ui, c, snap),
            Some(Selection::Track(t)) => self.track_menu(ui, t, snap),
            None => {
                if ui.button("Split everything at playhead    S").clicked() {
                    self.split_at(snap.playhead);
                    ui.close();
                }
                if ui
                    .add_enabled(self.doc.can_undo(), egui::Button::new("Undo    Ctrl+Z"))
                    .clicked()
                {
                    self.undo();
                    ui.close();
                }
            }
        });
    }

    fn paint_timeline(&self, ui: &egui::Ui, rect: Rect, snap: &Snapshot) {
        let painter = ui.painter_at(rect);
        let visible = ui.clip_rect().intersect(rect);
        let pps = self.px_per_sec;
        let sr = snap.sr;
        let x_of = |s: usize| rect.left() + s as f32 / sr * pps;
        let row_top = |i: usize| rect.top() + RULER_H + i as f32 * ROW_H;

        // Rows.
        for i in 0..self.doc.tracks.len() + usize::from(snap.recording) {
            let r = Rect::from_min_size(
                pos2(visible.left(), row_top(i)),
                vec2(visible.width(), ROW_H),
            );
            painter.rect_filled(r.shrink2(vec2(0.0, 2.0)), 0.0, row_bg(ui, i));
        }

        // Beat grid when the click or snap is on, otherwise a seconds grid.
        let text_col = ui.visuals().weak_text_color();
        let grid_col = ui
            .visuals()
            .widgets
            .noninteractive
            .bg_stroke
            .color
            .gamma_multiply(0.6);
        let beats = snap.knobs.metronome || self.snap_to_beat;
        let (step, label_every) = if beats {
            (60.0 / snap.knobs.bpm.max(20.0), 4)
        } else {
            let s = [0.5, 1.0, 2.0, 5.0, 10.0, 30.0]
                .into_iter()
                .find(|s| s * pps >= 60.0)
                .unwrap_or(60.0);
            (s, 1)
        };
        let first = ((visible.left() - rect.left()) / pps / step)
            .floor()
            .max(0.0) as usize;
        let last = ((visible.right() - rect.left()) / pps / step).ceil() as usize;
        for k in first..=last {
            let t = k as f32 * step;
            let x = rect.left() + t * pps;
            let strong = k % label_every == 0;
            painter.line_segment(
                [pos2(x, rect.top() + RULER_H * 0.5), pos2(x, rect.bottom())],
                Stroke::new(
                    1.0,
                    if strong {
                        grid_col
                    } else {
                        grid_col.gamma_multiply(0.4)
                    },
                ),
            );
            if strong {
                let label = if beats {
                    format!("{}", k / 4 + 1)
                } else {
                    fmt_time_short(t)
                };
                painter.text(
                    pos2(x + 3.0, rect.top() + 2.0),
                    Align2::LEFT_TOP,
                    label,
                    FontId::monospace(11.0),
                    text_col,
                );
            }
        }

        // Clips.
        let any_solo = self.doc.tracks.iter().any(|t| t.solo);
        for (i, t) in self.doc.tracks.iter().enumerate() {
            let top = row_top(i) + 4.0;
            let muted = t.mute || (any_solo && !t.solo);
            let col = if muted {
                Color32::GRAY
            } else {
                TRACK_COLOURS[i % TRACK_COLOURS.len()]
            };
            for c in &t.clips {
                let r = Rect::from_min_max(
                    pos2(x_of(c.start), top),
                    pos2(x_of(c.end()), top + ROW_H - 8.0),
                );
                if r.right() < visible.left() || r.left() > visible.right() {
                    continue;
                }
                let selected = self.sel == Some(Selection::Clip(c.id));
                painter.rect_filled(
                    r,
                    4.0,
                    col.gamma_multiply(if selected { 0.32 } else { 0.18 }),
                );
                draw_wave(&painter, r, visible, &c.peaks, c.offset, sr / pps, col);
                let stroke = if selected {
                    Stroke::new(2.0, SELECT)
                } else {
                    Stroke::new(1.0, col.gamma_multiply(0.6))
                };
                painter.rect_stroke(r, 4.0, stroke, egui::StrokeKind::Inside);
                if r.width() > 60.0 {
                    painter.text(
                        r.left_top() + vec2(6.0, 3.0),
                        Align2::LEFT_TOP,
                        if t.invert {
                            format!("Ø {}", t.name)
                        } else {
                            t.name.clone()
                        },
                        FontId::proportional(11.0),
                        col.gamma_multiply(0.9),
                    );
                }
            }
        }

        // The take being recorded, drawn live.
        if snap.recording {
            let top = row_top(self.doc.tracks.len()) + 4.0;
            let r = Rect::from_min_max(
                pos2(x_of(snap.rec_start), top),
                pos2(
                    x_of(snap.rec_start + snap.rec_len).max(x_of(snap.rec_start) + 2.0),
                    top + ROW_H - 8.0,
                ),
            );
            painter.rect_filled(r, 4.0, REC_RED.gamma_multiply(0.22));
            draw_wave(&painter, r, visible, &self.live.peaks, 0, sr / pps, REC_RED);
            if snap.counting_in {
                painter.text(
                    rect.left_top() + vec2(x_of(snap.playhead) - rect.left() + 12.0, RULER_H + 8.0),
                    Align2::LEFT_TOP,
                    "Count-in…",
                    FontId::proportional(22.0),
                    REC_RED,
                );
            }
        }

        // Playhead.
        let x = x_of(snap.playhead);
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(
                2.0,
                if snap.recording {
                    REC_RED
                } else {
                    Color32::WHITE
                },
            ),
        );
        painter.add(egui::Shape::convex_polygon(
            vec![
                pos2(x - 6.0, rect.top()),
                pos2(x + 6.0, rect.top()),
                pos2(x, rect.top() + 8.0),
            ],
            Color32::WHITE,
            Stroke::NONE,
        ));
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        // A clean exit means there's nothing to recover next time.
        project::clear_autosave();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Some(err) = self.errors.lock().take() {
            self.status = err;
        }
        let snap = self.snapshot();
        self.in_meter = (self.in_meter * 0.85).max(snap.in_peak);
        self.out_meter = (self.out_meter * 0.85).max(snap.out_peak);
        let mut knobs = snap.knobs.clone();
        let mut actions = Vec::new();

        self.keyboard(&ctx, &snap, &mut actions);

        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New project").clicked() {
                        self.new_project();
                    }
                    if ui.button("Open project…").clicked() {
                        self.open_project();
                    }
                    if ui.button("Save project    Ctrl+S").clicked() {
                        self.save_project(false);
                    }
                    if ui.button("Save project as…").clicked() {
                        self.save_project(true);
                    }
                    ui.separator();
                    if ui.button("Import WAV as track…").clicked() {
                        self.import_wav();
                    }
                    if ui.button("Share / Export…").clicked() {
                        self.open_share();
                    }
                });
                ui.menu_button("Edit", |ui| {
                    if ui
                        .add_enabled(self.doc.can_undo(), egui::Button::new("Undo    Ctrl+Z"))
                        .clicked()
                    {
                        self.undo();
                    }
                    if ui
                        .add_enabled(
                            self.doc.can_redo(),
                            egui::Button::new("Redo    Ctrl+Shift+Z"),
                        )
                        .clicked()
                    {
                        self.redo();
                    }
                    ui.separator();
                    let has = self.sel.is_some();
                    if ui.button("Split at playhead    S").clicked() {
                        self.split_at(snap.playhead);
                    }
                    if ui
                        .add_enabled(has, egui::Button::new("Duplicate    Ctrl+D"))
                        .clicked()
                    {
                        self.duplicate_selection();
                    }
                    if ui
                        .add_enabled(has, egui::Button::new("Delete    Del"))
                        .clicked()
                    {
                        self.delete_selection();
                    }
                });
                if ui.button("Share").clicked() {
                    self.open_share();
                }
                if ui.button("Audio settings").clicked() {
                    self.show_audio = true;
                }
                if ui.button("Shortcuts").on_hover_text("F1").clicked() {
                    self.show_help = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("Unplugged").color(AMBER).strong());
                });
            });
        });
        egui::Panel::top("transport").show(ui, |ui| {
            ui.add_space(4.0);
            self.transport(ui, &snap, &mut knobs, &mut actions);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.status).small());
                if let Some(a) = &self.audio {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(&a.description).small().weak());
                    });
                }
            });
        });
        egui::Panel::left("side")
            .resizable(false)
            .exact_size(250.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.side_panel(ui, &mut knobs));
            });
        egui::CentralPanel::default().show(ui, |ui| self.timeline(ui, &snap, &mut actions));

        self.audio_window(&ctx);
        self.share_window(&ctx, &snap);
        self.help_window(&ctx);
        self.recovery_window(&ctx);
        self.close_guard(&ctx);
        self.tick_autosave(&snap);

        if knobs != snap.knobs {
            self.apply_knobs(&knobs);
        }
        for a in actions {
            self.handle(a, &snap);
        }
        if self.doc.revision != self.synced_rev {
            self.synced_rev = self.doc.revision;
            let mut e = self.engine.lock();
            e.set_tracks(&self.doc.tracks);
            e.set_input_fx(&self.doc.input_fx);
            e.set_master_fx(&self.doc.master_fx);
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

/// An on/off button that always looks like a button.
fn toggle(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let r = ui.add(egui::Button::new(label).selected(*on));
    if r.clicked() {
        *on = !*on;
    }
    r
}

/// Draws a min/max waveform for the part of `peaks` starting at sample `offset`.
fn draw_wave(
    painter: &egui::Painter,
    clip_rect: Rect,
    visible: Rect,
    peaks: &[[f32; 2]],
    offset: usize,
    samples_per_px: f32,
    col: Color32,
) {
    let mid = clip_rect.center().y;
    let half = clip_rect.height() * 0.42;
    let x0 = clip_rect.left().max(visible.left()).floor();
    let x1 = clip_rect.right().min(visible.right()).ceil();
    let buckets_per_px = samples_per_px / PEAK_BUCKET as f32;
    let first = offset as f32 / PEAK_BUCKET as f32;
    let mut x = x0;
    while x < x1 {
        let b0 = (first + (x - clip_rect.left()) * buckets_per_px) as usize;
        let b1 = ((first + (x + 1.0 - clip_rect.left()) * buckets_per_px) as usize)
            .max(b0 + 1)
            .min(peaks.len());
        if b0 >= peaks.len() {
            break;
        }
        let (lo, hi) = peaks[b0..b1]
            .iter()
            .fold((0.0f32, 0.0f32), |(l, h), p| (l.min(p[0]), h.max(p[1])));
        painter.line_segment(
            [
                Pos2::new(x, mid - hi.min(1.0) * half),
                Pos2::new(x, mid - lo.max(-1.0) * half + 1.0),
            ],
            Stroke::new(1.0, col),
        );
        x += 1.0;
    }
}

fn meter(ui: &mut egui::Ui, label: &str, level: f32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).small());
        let (r, _) =
            ui.allocate_exact_size(vec2(ui.available_width() - 40.0, 10.0), Sense::hover());
        ui.painter()
            .rect_filled(r, 2.0, ui.visuals().extreme_bg_color);
        let db = 20.0 * level.max(1e-5).log10();
        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let col = if db > -1.0 {
            REC_RED
        } else if db > -12.0 {
            AMBER
        } else {
            Color32::from_rgb(90, 200, 120)
        };
        ui.painter().rect_filled(
            Rect::from_min_size(r.min, vec2(r.width() * frac, r.height())),
            2.0,
            col,
        );
        ui.label(
            RichText::new(if level > 1e-5 {
                format!("{db:.0}")
            } else {
                "-∞".into()
            })
            .small()
            .monospace(),
        );
    });
}

fn row_bg(ui: &egui::Ui, i: usize) -> Color32 {
    let base = ui.visuals().faint_bg_color;
    if i.is_multiple_of(2) {
        base
    } else {
        ui.visuals().extreme_bg_color
    }
}

fn fmt_time(secs: f32) -> String {
    let m = (secs / 60.0) as u32;
    let s = secs - m as f32 * 60.0;
    format!("{m:02}:{s:06.3}")
}

fn fmt_time_short(secs: f32) -> String {
    let m = (secs / 60.0) as u32;
    let s = secs - m as f32 * 60.0;
    if s.fract() == 0.0 {
        format!("{m}:{:02}", s as u32)
    } else {
        format!("{m}:{s:04.1}")
    }
}
