//! Offscreen UI tests: drive the app with a demo song (no audio devices needed).
//! Screenshots land in `target/ui-shots/`.
//!
//! Demo layout at 1400x800: Rhythm row centre y≈135, Lead row y≈210, timeline x=558 is
//! 0:00 at 60 px/s. Rhythm clip spans 0–12 s, Lead clips 3–6 s and 6–9 s.

use egui::{Event, Key, Modifiers, PointerButton, Pos2, Vec2, pos2};
use egui_kittest::{Harness, HarnessBuilder};
use std::collections::VecDeque;
use unplugged::app::App;
use unplugged::model::Selection;

const X0: f32 = 558.0;
const PPS: f32 = 60.0;
const RHYTHM_Y: f32 = 135.0;
const LEAD_Y: f32 = 210.0;

fn x_at(secs: f32) -> f32 {
    X0 + secs * PPS
}

fn harness() -> Harness<'static, App> {
    let mut h = HarnessBuilder::default()
        .with_size(Vec2::new(1400.0, 800.0))
        .wgpu()
        .build_eframe(|cc| {
            let mut app = App::new_headless(&cc.egui_ctx);
            app.load_demo();
            app
        });
    steps(&mut h, 4);
    h
}

fn steps(h: &mut Harness<'_, App>, n: usize) {
    for _ in 0..n {
        h.step();
    }
}

fn button(h: &mut Harness<'_, App>, pos: Pos2, button: PointerButton, pressed: bool) {
    h.event(Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: Modifiers::NONE,
    });
    h.step();
}

fn click(h: &mut Harness<'_, App>, pos: Pos2, b: PointerButton) {
    h.event(Event::PointerMoved(pos));
    h.step();
    button(h, pos, b, true);
    button(h, pos, b, false);
    steps(h, 2);
}

fn key(h: &mut Harness<'_, App>, k: Key, m: Modifiers) {
    h.key_press_modifiers(m, k);
    steps(h, 2);
}

fn shot(h: &mut Harness<'_, App>, name: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ui-shots");
    std::fs::create_dir_all(&dir).unwrap();
    h.render()
        .expect("render")
        .save(dir.join(format!("{name}.png")))
        .unwrap();
}

fn clips(h: &Harness<'_, App>, track: usize) -> usize {
    h.state().doc().tracks[track].clips.len()
}

#[test]
fn renders_main_window() {
    let mut h = harness();
    shot(&mut h, "main");
}

#[test]
fn click_select_delete_and_undo_clip() {
    let mut h = harness();
    click(&mut h, pos2(x_at(1.0), RHYTHM_Y), PointerButton::Primary);
    assert!(matches!(h.state().selection(), Some(Selection::Clip(_))));
    key(&mut h, Key::Delete, Modifiers::NONE);
    assert_eq!(clips(&h, 0), 0, "Delete key removes the selected clip");
    key(&mut h, Key::Z, Modifiers::COMMAND);
    assert_eq!(clips(&h, 0), 1, "Ctrl+Z brings it back");
    key(&mut h, Key::Z, Modifiers::COMMAND | Modifiers::SHIFT);
    assert_eq!(clips(&h, 0), 0, "Ctrl+Shift+Z redoes");
}

#[test]
fn select_track_header_and_delete_track() {
    let mut h = harness();
    click(&mut h, pos2(530.0, LEAD_Y + 22.0), PointerButton::Primary);
    assert!(matches!(h.state().selection(), Some(Selection::Track(_))));
    key(&mut h, Key::Backspace, Modifiers::NONE);
    assert_eq!(h.state().doc().tracks.len(), 1);
    key(&mut h, Key::Z, Modifiers::COMMAND);
    assert_eq!(h.state().doc().tracks.len(), 2, "undo delete track");
}

#[test]
fn right_click_menus() {
    let mut h = harness();
    click(&mut h, pos2(x_at(4.0), LEAD_Y), PointerButton::Secondary);
    shot(&mut h, "clip_menu");
    key(&mut h, Key::Escape, Modifiers::NONE);
    click(&mut h, pos2(400.0, RHYTHM_Y), PointerButton::Secondary);
    shot(&mut h, "track_menu");
}

#[test]
fn split_at_playhead_with_s() {
    let mut h = harness();
    click(&mut h, pos2(x_at(5.0), 78.0), PointerButton::Primary); // ruler: seek to 5 s
    click(&mut h, pos2(x_at(1.0), RHYTHM_Y), PointerButton::Primary);
    key(&mut h, Key::S, Modifiers::NONE);
    assert_eq!(clips(&h, 0), 2);
    let starts: Vec<usize> = h.state().doc().tracks[0]
        .clips
        .iter()
        .map(|c| c.start)
        .collect();
    assert!(
        (starts[1] as f32 / 48000.0 - 5.0).abs() < 0.05,
        "split near 5 s: {starts:?}"
    );
}

#[test]
fn drag_clip_to_another_track() {
    let mut h = harness();
    let from = pos2(x_at(7.5), LEAD_Y);
    let to = pos2(x_at(13.5), RHYTHM_Y);
    h.event(Event::PointerMoved(from));
    h.step();
    button(&mut h, from, PointerButton::Primary, true);
    for i in 1..=10 {
        let t = i as f32 / 10.0;
        h.event(Event::PointerMoved(from + (to - from) * t));
        h.step();
    }
    shot(&mut h, "dragging");
    button(&mut h, to, PointerButton::Primary, false);
    steps(&mut h, 2);
    assert_eq!(clips(&h, 0), 2, "clip landed on the Rhythm track");
    assert_eq!(clips(&h, 1), 1);
    let moved = h.state().doc().tracks[0].clips[1].start as f32 / 48000.0;
    assert!(
        (moved - 12.0).abs() < 0.1,
        "moved by 6 s to ~12 s, got {moved}"
    );
    key(&mut h, Key::Z, Modifiers::COMMAND);
    assert_eq!(clips(&h, 1), 2, "one undo restores the whole drag");
}

#[test]
fn trim_clip_edge() {
    let mut h = harness();
    // Right edge of the Rhythm clip (12 s) dragged back to 10 s.
    let from = pos2(x_at(12.0) - 3.0, RHYTHM_Y);
    let to = pos2(x_at(10.0), RHYTHM_Y);
    h.event(Event::PointerMoved(from));
    h.step();
    button(&mut h, from, PointerButton::Primary, true);
    for i in 1..=6 {
        h.event(Event::PointerMoved(from + (to - from) * (i as f32 / 6.0)));
        h.step();
    }
    button(&mut h, to, PointerButton::Primary, false);
    let end = h.state().doc().tracks[0].clips[0].end() as f32 / 48000.0;
    assert!((end - 10.0).abs() < 0.1, "trimmed to ~10 s, got {end}");
}

#[test]
fn live_waveform_while_recording() {
    let mut h = harness();
    let engine = h.state().engine();
    {
        let mut e = engine.lock();
        e.monitor = false;
        e.seek(48000);
        e.record(Vec::new(), 0);
        let mut input: VecDeque<f32> = (0..48000 * 3)
            .map(|i| (i as f32 * 0.03).sin() * (0.3 + 0.3 * ((i / 12000) % 2) as f32))
            .collect();
        let mut out = vec![0.0; 48000 * 3 * 2];
        e.render(&mut input, &mut out);
    }
    steps(&mut h, 3);
    shot(&mut h, "recording");
    assert!(engine.lock().recording_len() == 48000 * 3);
}

#[test]
fn capture_turns_recent_playing_into_a_take() {
    let mut h = harness();
    let engine = h.state().engine();
    {
        // 3 s of silence then 2 s of "guitar" while stopped.
        let mut e = engine.lock();
        e.monitor = false;
        let mut input: VecDeque<f32> = (0..48000 * 5)
            .map(|i| {
                if i < 48000 * 3 {
                    0.0
                } else {
                    (i as f32 * 0.05).sin() * 0.5
                }
            })
            .collect();
        let mut out = vec![0.0; 48000 * 5 * 2];
        e.render(&mut input, &mut out);
    }
    key(&mut h, Key::C, Modifiers::NONE);
    let doc = h.state().doc();
    assert_eq!(doc.tracks.len(), 3, "capture adds a track");
    let len = doc.tracks[2].clips[0].len as f32 / 48000.0;
    assert!(
        (len - 2.2).abs() < 0.05,
        "silence trimmed, ~2 s + padding kept: {len}"
    );
    assert_eq!(
        doc.tracks[2].clips[0].start, 0,
        "stopped: lands at the playhead"
    );
    shot(&mut h, "capture");
}

#[test]
fn toggles_look_like_buttons() {
    let mut h = harness();
    h.state().engine().lock().monitor = false;
    steps(&mut h, 2);
    shot(&mut h, "toggles_off");
}

#[test]
fn add_effect_from_menu_and_undo() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness();
    h.get_by_label("Master").click();
    steps(&mut h, 2);
    h.get_by_label("➕ Add effect").click();
    steps(&mut h, 2);
    h.get_by_label("Reverb").click();
    steps(&mut h, 2);
    let kinds: Vec<_> = h.state().doc().master_fx.iter().map(|f| f.kind).collect();
    assert_eq!(
        kinds,
        [
            unplugged::fx::FxKind::Limiter,
            unplugged::fx::FxKind::Reverb
        ]
    );
    shot(&mut h, "master_fx");
    key(&mut h, Key::Z, Modifiers::COMMAND);
    assert_eq!(
        h.state().doc().master_fx.len(),
        1,
        "undo removes the added effect"
    );
}
