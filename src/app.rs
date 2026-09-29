use crate::audio::{self, DeviceInfo, ErrorSlot, InputQueue, RunningAudio};
use crate::dsp::{IrSpectrum, SimParams, builtin_body_ir};
use crate::engine::{Engine, PEAK_BUCKET, TrackView, render_mix};
use crate::project;
use cpal::HostId;
use egui::{
    Align2, Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, UiBuilder, pos2, vec2,
};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const ROW_H: f32 = 84.0;
const RULER_H: f32 = 24.0;
const CTRL_W: f32 = 290.0;

const AMBER: Color32 = Color32::from_rgb(232, 170, 80);
const BLUE: Color32 = Color32::from_rgb(110, 160, 230);
const REC_RED: Color32 = Color32::from_rgb(220, 60, 60);

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
    record_acoustic: bool,
}

/// Everything the UI needs from the engine for one frame, copied out under a short lock.
struct Snapshot {
    tracks: Vec<TrackView>,
    playing: bool,
    recording: bool,
    playhead: usize,
    rec_start: usize,
    rec_len: usize,
    sr: f32,
    knobs: Knobs,
    ir_name: String,
    in_peak: f32,
    out_peak: f32,
}

enum Action {
    PlayPause,
    Record,
    Rewind,
    Seek(usize),
    Track(TrackView),
    Remove(u64),
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

    sim: SimParams,
    status: String,
    px_per_sec: f32,
    follow: bool,
    scroll_x: f32,
    view_w: f32,
    in_meter: f32,
    out_meter: f32,
    project_dir: Option<PathBuf>,
    take_counter: usize,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.selection.bg_fill = Color32::from_rgb(150, 100, 40);
        visuals.hyperlink_color = AMBER;
        cc.egui_ctx.set_visuals(visuals);

        let host = cpal::default_host().id();
        let mut app = Self {
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
            buffer: None,
            audio: None,
            show_audio: false,
            sim: SimParams::default(),
            status: String::new(),
            px_per_sec: 60.0,
            follow: true,
            scroll_x: 0.0,
            view_w: 800.0,
            in_meter: 0.0,
            out_meter: 0.0,
            project_dir: None,
            take_counter: 0,
        };
        app.refresh_devices();
        app.start_audio(&cc.egui_ctx);
        app
    }

    fn refresh_devices(&mut self) {
        let (inputs, outputs) = audio::list_devices(self.host);
        let (def_in, def_out) = audio::default_indices(self.host, &inputs, &outputs);
        self.in_idx = audio::pick_scarlett(&inputs).unwrap_or(def_in);
        self.out_idx = audio::pick_scarlett(&outputs).unwrap_or(def_out);
        self.in_channel = inputs
            .get(self.in_idx)
            .map_or(0, |d| audio::default_input_channel(&d.name));
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

    fn snapshot(&self) -> Snapshot {
        let mut e = self.engine.lock();
        let s = Snapshot {
            tracks: e.views(),
            playing: e.playing,
            recording: e.recording,
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
                record_acoustic: e.record_acoustic,
            },
            ir_name: e.ir_name().to_string(),
            in_peak: e.in_peak,
            out_peak: e.out_peak,
        };
        e.in_peak = 0.0;
        e.out_peak = 0.0;
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
        e.record_acoustic = k.record_acoustic;
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
                    self.engine.lock().record(buf);
                }
            }
            Action::Rewind => self.engine.lock().seek(0),
            Action::Seek(p) => self.engine.lock().seek(p),
            Action::Track(v) => self.engine.lock().update_track(&v),
            Action::Remove(id) => self.engine.lock().remove_track(id),
        }
    }

    fn stop(&mut self) {
        let take = self.engine.lock().stop();
        if let Some((start, samples)) = take {
            self.take_counter += 1;
            let acoustic = self.engine.lock().record_acoustic;
            let view = project::track_view(
                format!("Take {}", self.take_counter),
                samples,
                start,
                0.8,
                0.0,
                false,
                false,
                acoustic,
            );
            let mut e = self.engine.lock();
            let sim = e.new_sim();
            e.add_track(view, sim);
        }
    }

    fn add_views(&mut self, views: Vec<TrackView>) {
        let mut e = self.engine.lock();
        for v in views {
            let sim = e.new_sim();
            e.add_track(v, sim);
        }
    }

    fn set_ir(&mut self, path: Option<PathBuf>) {
        let sr = self.engine.lock().sr;
        let ir = match &path {
            None => IrSpectrum::new("Built-in body", &builtin_body_ir(sr)),
            Some(p) => match project::load_ir(p, sr as u32) {
                Ok(ir) => IrSpectrum::new(
                    p.file_stem()
                        .map_or("IR".into(), |s| s.to_string_lossy().to_string()),
                    &ir,
                ),
                Err(e) => {
                    self.status = format!("Couldn't load IR: {e:#}");
                    return;
                }
            },
        };
        self.sim.ir_path = path.map(|p| p.to_string_lossy().to_string());
        let mut e = self.engine.lock();
        e.set_ir(Arc::new(ir));
        e.set_params(&self.sim);
    }

    // ---- file operations --------------------------------------------------

    fn new_project(&mut self) {
        self.stop();
        let mut e = self.engine.lock();
        e.clear_tracks();
        e.seek(0);
        drop(e);
        self.project_dir = None;
        self.take_counter = 0;
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
        let (tracks, sr, bpm) = {
            let e = self.engine.lock();
            (e.views(), e.sr as u32, e.bpm)
        };
        self.status = match project::save(&dir, sr, bpm, &self.sim, &tracks) {
            Ok(()) => format!("Saved to {}", dir.display()),
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
        match project::load(&dir, sr) {
            Ok((pf, views)) => {
                self.new_project();
                self.take_counter = views.len();
                self.add_views(views);
                self.engine.lock().bpm = pf.bpm;
                self.sim = pf.sim;
                self.set_ir(self.sim.ir_path.clone().map(PathBuf::from));
                self.status = format!("Opened {}", dir.display());
                self.project_dir = Some(dir);
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
                self.add_views(vec![project::track_view(
                    name, samples, at, 0.8, 0.0, false, false, false,
                )]);
                self.status = format!("Imported {}", path.display());
            }
            Err(e) => self.status = format!("Import failed: {e:#}"),
        }
    }

    fn export_mix(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("WAV audio", &["wav"])
            .set_file_name("mix.wav")
            .save_file()
        else {
            return;
        };
        let (tracks, ir, sr) = {
            let e = self.engine.lock();
            (e.views(), e.ir(), e.sr)
        };
        let mix = render_mix(&tracks, &self.sim, ir, sr);
        self.status = match project::export_mix(&path, &mix, sr as u32) {
            Ok(()) => format!("Exported {}", path.display()),
            Err(e) => format!("Export failed: {e:#}"),
        };
    }

    // ---- UI pieces ---------------------------------------------------------

    fn audio_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_audio;
        let mut restart = false;
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

                        ui.label("Output (headphones)");
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
                    if ui.button("Apply / restart audio").clicked() {
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
                    "Scarlett Solo: the guitar jack is Input 2. Turn the Direct Monitor button OFF \
                     or you'll hear the dry electric under the acoustic sound.",
                )
                .small()
                .weak(),
            );
            });
        self.show_audio = open;
        if restart {
            self.stop();
            self.start_audio(ctx);
        }
    }

    fn sim_panel(&mut self, ui: &mut egui::Ui, snap: &Snapshot, knobs: &mut Knobs) {
        ui.heading("Acoustic sim");
        ui.label(
            RichText::new("Makes the Strat sound like an acoustic")
                .small()
                .weak(),
        );
        ui.add_space(6.0);
        let before = self.sim.clone();
        ui.checkbox(&mut self.sim.enabled, "Enabled");
        ui.add_enabled_ui(self.sim.enabled, |ui| {
            ui.checkbox(&mut self.sim.pickup_eq, "Pickup correction EQ")
                .on_hover_text("Cuts the mid honk and pickup resonance of magnetic pickups");
            ui.add(egui::Slider::new(&mut self.sim.body, 0.0..=1.0).text("Body"));
            ui.add(egui::Slider::new(&mut self.sim.brightness_db, -6.0..=14.0).text("Sparkle dB"));
            ui.add(egui::Slider::new(&mut self.sim.warmth_db, -6.0..=10.0).text("Warmth dB"));
            ui.add(egui::Slider::new(&mut self.sim.room, 0.0..=1.0).text("Room"));
            ui.add(egui::Slider::new(&mut self.sim.level_db, -18.0..=12.0).text("Level dB"));
            ui.add_space(4.0);
            ui.label(format!("Body IR: {}", snap.ir_name));
            ui.horizontal(|ui| {
                if ui
                    .button("Load IR…")
                    .on_hover_text("Any acoustic-sim impulse response .wav")
                    .clicked()
                {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("WAV", &["wav"])
                        .pick_file()
                    {
                        self.set_ir(Some(p));
                    }
                }
                if ui.button("Built-in").clicked() {
                    self.set_ir(None);
                }
            });
            if ui.button("Reset knobs").clicked() {
                self.sim = SimParams {
                    ir_path: self.sim.ir_path.clone(),
                    ..SimParams::default()
                };
            }
        });
        if self.sim != before {
            self.engine.lock().set_params(&self.sim);
        }

        ui.separator();
        ui.heading("Input");
        ui.checkbox(&mut knobs.monitor, "Hear myself (monitor)");
        ui.add(egui::Slider::new(&mut knobs.input_gain_db, -12.0..=24.0).text("Gain dB"));
        meter(ui, "In", self.in_meter);
        meter(ui, "Out", self.out_meter);

        ui.separator();
        ui.heading("Recording");
        ui.checkbox(&mut knobs.record_acoustic, "New takes use acoustic sim")
            .on_hover_text("Takes are always recorded clean; this only sets how they play back. You can flip it per track.");
        ui.add(egui::Slider::new(&mut knobs.latency_ms, 0.0..=80.0).text("Latency ms"))
            .on_hover_text(
                "If new takes sound late against old ones, raise this. If early, lower it.",
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
                RichText::new("⏺ Stop rec").size(18.0).color(Color32::WHITE)
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

            ui.add_space(10.0);
            ui.label(
                RichText::new(fmt_time(snap.playhead as f32 / snap.sr))
                    .monospace()
                    .size(20.0),
            );
            ui.add_space(10.0);
            ui.separator();
            ui.checkbox(&mut knobs.metronome, "Click");
            ui.add(
                egui::DragValue::new(&mut knobs.bpm)
                    .range(30.0..=300.0)
                    .suffix(" bpm")
                    .speed(0.5),
            );
            ui.add(egui::Slider::new(&mut knobs.click_volume, 0.0..=1.0).show_value(false))
                .on_hover_text("Click volume");
            ui.separator();
            ui.label("Master");
            ui.add(egui::Slider::new(&mut knobs.master, 0.0..=1.5).show_value(false));
            ui.separator();
            ui.checkbox(&mut self.follow, "Follow");
            ui.label("Zoom");
            ui.add(
                egui::Slider::new(&mut self.px_per_sec, 10.0..=600.0)
                    .logarithmic(true)
                    .show_value(false),
            );
        });
    }

    fn timeline(&mut self, ui: &mut egui::Ui, snap: &Snapshot, actions: &mut Vec<Action>) {
        if snap.tracks.is_empty() && !snap.recording {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No tracks yet").size(22.0));
                ui.label("Hit ⏺ Rec (or press R) and play. Every recording becomes a new layer.");
                ui.label("Record again to layer on top while the earlier takes play back.");
            });
            return;
        }
        let rows = snap.tracks.len() + usize::from(snap.recording);
        let total_h = RULER_H + rows as f32 * ROW_H;
        let sr = snap.sr;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    // Track control strip.
                    let (col, _) = ui.allocate_exact_size(vec2(CTRL_W, total_h), Sense::hover());
                    for (i, t) in snap.tracks.iter().enumerate() {
                        let r = Rect::from_min_size(
                            col.min + vec2(0.0, RULER_H + i as f32 * ROW_H),
                            vec2(CTRL_W, ROW_H),
                        );
                        ui.painter()
                            .rect_filled(r.shrink2(vec2(2.0, 2.0)), 4.0, row_bg(ui, i));
                        let mut child = ui.new_child(
                            UiBuilder::new()
                                .max_rect(r.shrink(8.0))
                                .layout(egui::Layout::top_down(egui::Align::Min)),
                        );
                        track_controls(&mut child, t, sr, actions);
                    }
                    if snap.recording {
                        let r = Rect::from_min_size(
                            col.min + vec2(0.0, RULER_H + snap.tracks.len() as f32 * ROW_H),
                            vec2(CTRL_W, ROW_H),
                        );
                        ui.painter().text(
                            r.left_center() + vec2(12.0, 0.0),
                            Align2::LEFT_CENTER,
                            "⏺ Recording…",
                            FontId::proportional(16.0),
                            REC_RED,
                        );
                    }

                    // Waveforms.
                    let len_samples = snap
                        .tracks
                        .iter()
                        .map(|t| t.end())
                        .max()
                        .unwrap_or(0)
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
                        && (playhead_x < self.scroll_x
                            || playhead_x > self.scroll_x + self.view_w - 40.0)
                    {
                        area = area.horizontal_scroll_offset((playhead_x - 40.0).max(0.0));
                    }
                    let out = area.show(ui, |ui| {
                        let (rect, resp) = ui
                            .allocate_exact_size(vec2(content_w, total_h), Sense::click_and_drag());
                        // Ctrl + wheel zooms.
                        if resp.hovered() {
                            let (ctrl, dy) =
                                ui.input(|i| (i.modifiers.ctrl, i.smooth_scroll_delta.y));
                            if ctrl && dy != 0.0 {
                                self.px_per_sec =
                                    (self.px_per_sec * (1.0 + dy * 0.003)).clamp(10.0, 600.0);
                            }
                        }
                        if let Some(p) = resp
                            .interact_pointer_pos()
                            .filter(|_| resp.clicked() || resp.dragged())
                        {
                            let x = (p.x - rect.left()).max(0.0);
                            actions.push(Action::Seek((x / self.px_per_sec * sr) as usize));
                        }
                        self.paint_timeline(ui, rect, snap);
                    });
                    self.scroll_x = out.state.offset.x;
                    self.view_w = out.inner_rect.width();
                });
            });
    }

    fn paint_timeline(&self, ui: &egui::Ui, rect: Rect, snap: &Snapshot) {
        let painter = ui.painter_at(rect);
        let clip = ui.clip_rect().intersect(rect);
        let pps = self.px_per_sec;
        let sr = snap.sr;
        let x_of = |s: usize| rect.left() + s as f32 / sr * pps;

        // Rows.
        for i in 0..snap.tracks.len() + usize::from(snap.recording) {
            let r = Rect::from_min_size(
                pos2(clip.left(), rect.top() + RULER_H + i as f32 * ROW_H),
                vec2(clip.width(), ROW_H),
            );
            painter.rect_filled(r.shrink2(vec2(0.0, 2.0)), 0.0, row_bg(ui, i));
        }

        // Beat grid when the click is on, otherwise a seconds grid.
        let text_col = ui.visuals().weak_text_color();
        let grid_col = ui
            .visuals()
            .widgets
            .noninteractive
            .bg_stroke
            .color
            .gamma_multiply(0.6);
        let (step, label_every) = if snap.knobs.metronome {
            (60.0 / snap.knobs.bpm.max(20.0), 4)
        } else {
            let s = [0.5, 1.0, 2.0, 5.0, 10.0, 30.0]
                .into_iter()
                .find(|s| s * pps >= 60.0)
                .unwrap_or(60.0);
            (s, 1)
        };
        let first = ((clip.left() - rect.left()) / pps / step).floor().max(0.0) as usize;
        let last = ((clip.right() - rect.left()) / pps / step).ceil() as usize;
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
                let label = if snap.knobs.metronome {
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

        // Waveforms.
        for (i, t) in snap.tracks.iter().enumerate() {
            let top = rect.top() + RULER_H + i as f32 * ROW_H + 4.0;
            let clip_rect = Rect::from_min_max(
                pos2(x_of(t.start), top),
                pos2(x_of(t.end()), top + ROW_H - 8.0),
            );
            let muted = t.mute || (snap.tracks.iter().any(|t| t.solo) && !t.solo);
            let col = if muted {
                Color32::GRAY
            } else if t.acoustic {
                AMBER
            } else {
                BLUE
            };
            painter.rect_filled(clip_rect, 4.0, col.gamma_multiply(0.18));
            painter.rect_stroke(
                clip_rect,
                4.0,
                Stroke::new(1.0, col.gamma_multiply(0.6)),
                egui::StrokeKind::Inside,
            );
            draw_wave(&painter, clip_rect, clip, &t.peaks, sr / pps, col);
        }

        // Live recording region.
        if snap.recording {
            let top = rect.top() + RULER_H + snap.tracks.len() as f32 * ROW_H + 4.0;
            let r = Rect::from_min_max(
                pos2(x_of(snap.rec_start), top),
                pos2(x_of(snap.rec_start + snap.rec_len), top + ROW_H - 8.0),
            );
            painter.rect_filled(r, 4.0, REC_RED.gamma_multiply(0.35));
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

        if !ctx.egui_wants_keyboard_input() {
            ctx.input(|i| {
                if i.key_pressed(Key::Home) {
                    actions.push(Action::Rewind);
                }
                if i.key_pressed(Key::Space) {
                    actions.push(Action::PlayPause);
                }
                if i.key_pressed(Key::R) {
                    actions.push(Action::Record);
                }
            });
        }

        egui::Panel::top("menu").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New project").clicked() {
                        self.new_project();
                    }
                    if ui.button("Open project…").clicked() {
                        self.open_project();
                    }
                    if ui.button("Save project").clicked() {
                        self.save_project(false);
                    }
                    if ui.button("Save project as…").clicked() {
                        self.save_project(true);
                    }
                    ui.separator();
                    if ui.button("Import WAV as track…").clicked() {
                        self.import_wav();
                    }
                    if ui.button("Export mix (WAV)…").clicked() {
                        self.export_mix();
                    }
                });
                if ui.button("Audio settings").clicked() {
                    self.show_audio = true;
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
        egui::Panel::left("sim")
            .resizable(false)
            .exact_size(250.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.sim_panel(ui, &snap, &mut knobs));
            });
        egui::CentralPanel::default().show(ui, |ui| self.timeline(ui, &snap, &mut actions));

        self.audio_window(&ctx);

        if knobs != snap.knobs {
            self.apply_knobs(&knobs);
        }
        for a in actions {
            self.handle(a, &snap);
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

fn track_controls(ui: &mut egui::Ui, t: &TrackView, sr: f32, actions: &mut Vec<Action>) {
    let mut v = t.clone();
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut v.name).desired_width(150.0));
        let toggle = |ui: &mut egui::Ui, on: &mut bool, label: &str, col: Color32, tip: &str| {
            let text = if *on {
                RichText::new(label).color(Color32::BLACK)
            } else {
                RichText::new(label)
            };
            let b = egui::Button::new(text).fill(if *on {
                col
            } else {
                ui.visuals().widgets.inactive.bg_fill
            });
            if ui.add(b).on_hover_text(tip).clicked() {
                *on = !*on;
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
        toggle(
            ui,
            &mut v.acoustic,
            "A",
            AMBER,
            "Play back through the acoustic sim",
        );
        if ui.button("🗑").on_hover_text("Delete track").clicked() {
            actions.push(Action::Remove(t.id));
        }
    });
    ui.horizontal(|ui| {
        ui.label("Vol");
        ui.add(egui::Slider::new(&mut v.volume, 0.0..=1.5).show_value(false));
        ui.label("Pan");
        ui.add(egui::Slider::new(&mut v.pan, -1.0..=1.0).show_value(false))
            .on_hover_text("Double-tap left/right to hard-pan layered takes");
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Nudge").small().weak());
        let per_ms = (sr / 1000.0) as i64;
        for (label, ms) in [("◀10ms", -10i64), ("◀1ms", -1), ("1ms▶", 1), ("10ms▶", 10)] {
            if ui.small_button(label).clicked() {
                v.start = (v.start as i64 + ms * per_ms).max(0) as usize;
            }
        }
    });
    let changed = v.name != t.name
        || v.volume != t.volume
        || v.pan != t.pan
        || v.mute != t.mute
        || v.solo != t.solo
        || v.acoustic != t.acoustic
        || v.start != t.start;
    if changed {
        actions.push(Action::Track(v));
    }
}

fn draw_wave(
    painter: &egui::Painter,
    clip_rect: Rect,
    visible: Rect,
    peaks: &[[f32; 2]],
    samples_per_px: f32,
    col: Color32,
) {
    let mid = clip_rect.center().y;
    let half = clip_rect.height() * 0.45;
    let x0 = clip_rect.left().max(visible.left()).floor();
    let x1 = clip_rect.right().min(visible.right()).ceil();
    let buckets_per_px = samples_per_px / PEAK_BUCKET as f32;
    let mut x = x0;
    while x < x1 {
        let b0 = ((x - clip_rect.left()) * buckets_per_px) as usize;
        let b1 = (((x + 1.0 - clip_rect.left()) * buckets_per_px) as usize)
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
    if i % 2 == 0 {
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
