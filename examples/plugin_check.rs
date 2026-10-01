//! Loads real plugins headlessly and checks the whole hosting path:
//! `cargo run --release --example plugin_check -- <plugin path> [<plugin path> ...]`
//! (VST3 bundles or .clap files). Plugin windows open hidden.

use std::collections::HashSet;
use unplugged::fx::{FxSlot, PluginMaker};
use unplugged::plugins::{self, PluginFormat, PluginHost, PluginRef};

fn energy(x: &[f32]) -> f32 {
    x.iter().map(|v| v * v).sum::<f32>()
}

fn main() {
    // SAFETY: single-threaded at this point.
    unsafe { std::env::set_var("UNPLUGGED_HIDDEN_PLUGIN_WINDOWS", "1") };
    let found = plugins::scan();
    println!("scan found {} plugin(s):", found.len());
    for p in &found {
        println!(
            "  {} [{}] {} {} {}",
            p.name,
            p.format.label(),
            if p.instrument { "instrument" } else { "effect" },
            p.vendor,
            p.path
        );
    }

    let mut host = PluginHost::default();
    host.set_sample_rate(48000.0);
    let paths: Vec<String> = std::env::args().skip(1).collect();
    for (n, path) in paths.iter().enumerate() {
        let format = if path.ends_with(".clap") {
            PluginFormat::Clap
        } else {
            PluginFormat::Vst3
        };
        // Use the scanned id for CLAP (and VST3 class id when known).
        let id = found
            .iter()
            .find(|f| f.path == *path || path.ends_with(&f.path))
            .map(|f| f.id.clone())
            .unwrap_or_default();
        let id = if format == PluginFormat::Clap && id.is_empty() {
            found
                .iter()
                .find(|f| {
                    f.format == PluginFormat::Clap
                        && std::path::Path::new(&f.path).file_name()
                            == std::path::Path::new(path).file_name()
                })
                .map(|f| f.id.clone())
                .unwrap_or_default()
        } else {
            id
        };
        let r = PluginRef {
            format,
            path: path.clone(),
            id,
            name: format!("test{n}"),
            vendor: String::new(),
            state: vec![],
        };
        let slot = FxSlot::new_plugin(1000 + n as u64, r.clone());
        println!("\n== {path} ({})", format.label());
        let Some(mut fx) = host.make_effect(&slot) else {
            println!("  LOAD FAILED: {:?}", host.failed.get(&slot.id));
            continue;
        };
        println!("  loaded ok, latency {} samples", fx.latency());

        // An impulse, then silence: a reverb should ring after it.
        let mut l = vec![0.0f32; 48000];
        let mut r_ = vec![0.0f32; 48000];
        l[0] = 1.0;
        r_[0] = 1.0;
        for (a, b) in l.chunks_mut(256).zip(r_.chunks_mut(256)) {
            fx.process_block(a, b);
        }
        println!(
            "  output energy after impulse: {:.4} (tail after 0.1 s: {:.6})",
            energy(&l) + energy(&r_),
            energy(&l[4800..])
        );
        println!(
            "  all finite: {}",
            l.iter().chain(&r_).all(|v| v.is_finite())
        );

        let state = host.save_state(slot.id);
        println!("  state: {:?} bytes", state.as_ref().map(Vec::len));
        println!("  has editor: {}", host.has_editor(slot.id));
        if host.has_editor(slot.id) {
            match host.open_editor(slot.id) {
                Ok(()) => {
                    for _ in 0..10 {
                        host.tick();
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    println!("  editor opened (hidden) ok: {}", host.editor_open(slot.id));
                    host.close_editor(slot.id);
                    println!("  editor closed: {}", !host.editor_open(slot.id));
                }
                Err(e) => println!("  editor FAILED: {e:#}"),
            }
        }
        // Engine lets go, host tears down; then bring it back with the saved state.
        drop(fx);
        host.retain(&HashSet::new());
        let mut r2 = r.clone();
        r2.state = state.unwrap_or_default();
        let slot2 = FxSlot::new_plugin(2000 + n as u64, r2);
        let make: PluginMaker = &mut |s| host.make_effect(s);
        let again = make(&slot2);
        println!(
            "  reload with saved state: {}",
            if again.is_some() { "ok" } else { "FAILED" }
        );
        drop(again);
        host.retain(&HashSet::new());
    }
    println!("\ndone");
}
