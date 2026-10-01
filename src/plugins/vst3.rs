//! VST3 hosting via [vst3-host](https://github.com/HelgeSverre/rust-vst3-host) (MIT).
//! The VST3 SDK itself has been MIT-licensed since October 2025.

use super::window::EditorWindow;
use super::{InstalledPlugin, MAX_BLOCK, PluginFormat, PluginRef};
use crate::fx::Effect;
use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use vst3_host::audio::AudioBuffers;
use vst3_host::{Plugin, Vst3Host};

/// Finds installed VST3 bundles. Names and types come from each bundle's
/// `moduleinfo.json` when it has one, so scanning doesn't have to run plugin code.
pub fn scan() -> Vec<InstalledPlugin> {
    let dirs = vst3_host::discovery::scan_standard_paths();
    let bundles = vst3_host::discovery::scan_directories(&dirs).unwrap_or_default();
    let mut found = Vec::new();
    for b in bundles {
        let classes = module_info(&b);
        if classes.is_empty() {
            found.push(InstalledPlugin {
                format: PluginFormat::Vst3,
                path: b.to_string_lossy().to_string(),
                id: String::new(),
                name: b
                    .file_stem()
                    .map_or("VST3".into(), |s| s.to_string_lossy().to_string()),
                vendor: String::new(),
                instrument: false,
            });
        }
        for (id, name, vendor, instrument) in classes {
            found.push(InstalledPlugin {
                format: PluginFormat::Vst3,
                path: b.to_string_lossy().to_string(),
                id,
                name,
                vendor,
                instrument,
            });
        }
    }
    found
}

/// (class id, name, vendor, is instrument) for each audio class in a bundle's moduleinfo.json.
fn module_info(bundle: &Path) -> Vec<(String, String, String, bool)> {
    let file = bundle
        .join("Contents")
        .join("Resources")
        .join("moduleinfo.json");
    let Ok(text) = std::fs::read_to_string(file) else {
        return vec![];
    };
    // moduleinfo.json is JSON5-ish; strip trailing commas so serde_json accepts it.
    let cleaned = text
        .replace(",\n}", "\n}")
        .replace(",\n]", "\n]")
        .replace(",\r\n}", "\r\n}")
        .replace(",\r\n]", "\r\n]");
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&cleaned) else {
        return vec![];
    };
    let vendor = v["Factory Info"]["Vendor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    v["Classes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["Category"].as_str() == Some("Audio Module Class"))
        .map(|c| {
            let subs: Vec<&str> = c["Sub Categories"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str())
                .collect();
            (
                c["CID"].as_str().unwrap_or_default().to_string(),
                c["Name"].as_str().unwrap_or("VST3").to_string(),
                c["Vendor"].as_str().map_or(vendor.clone(), str::to_string),
                subs.iter().any(|s| s.eq_ignore_ascii_case("Instrument")),
            )
        })
        .collect()
}

pub struct Vst3Instance {
    plugin: Arc<Mutex<Plugin>>,
    window: Option<EditorWindow>,
    sr: f32,
    pub name: String,
}

impl Vst3Instance {
    pub fn new(r: &PluginRef, state: &[u8], sr: f32) -> Result<Self> {
        let mut host = Vst3Host::builder()
            .sample_rate(sr as f64)
            .block_size(MAX_BLOCK)
            .input_channels(2)
            .output_channels(2)
            .build()
            .map_err(|e| anyhow!("{e}"))?;
        let path = PathBuf::from(&r.path);
        let mut plugin = if r.id.len() == 32 {
            host.load_plugin_class(&path, &r.id)
        } else {
            host.load_plugin(&path)
        }
        .map_err(|e| anyhow!("{} wouldn't start: {e}", r.name))?;
        if !state.is_empty() {
            let _ = plugin.load_state(state);
        }
        plugin
            .start_processing()
            .map_err(|e| anyhow!("{} couldn't start processing: {e}", r.name))?;
        Ok(Self {
            plugin: Arc::new(Mutex::new(plugin)),
            window: None,
            sr,
            name: r.name.clone(),
        })
    }

    /// The effect that runs this plugin on the audio thread.
    pub fn effect(&mut self, sr: f32) -> Result<Box<dyn Effect>> {
        if (self.sr - sr).abs() > 0.5 {
            let mut p = self.plugin.lock();
            let _ = p.stop_processing();
            p.reconfigure(sr as f64, MAX_BLOCK)
                .map_err(|e| anyhow!("{e}"))?;
            p.start_processing().map_err(|e| anyhow!("{e}"))?;
            self.sr = sr;
        }
        let latency = self.plugin.lock().latency_samples() as usize;
        Ok(Box::new(Vst3Fx {
            plugin: self.plugin.clone(),
            buffers: AudioBuffers::new(2, 2, MAX_BLOCK, sr as f64),
            latency,
        }))
    }

    pub fn save_state(&self) -> Option<Vec<u8>> {
        self.plugin.lock().save_state().ok()
    }

    pub fn has_editor(&self) -> bool {
        self.plugin.lock().has_editor()
    }

    pub fn editor_open(&self) -> bool {
        self.window.is_some()
    }

    pub fn open_editor(&mut self) -> Result<()> {
        if let Some(w) = &self.window {
            w.focus();
            return Ok(());
        }
        let mut p = self.plugin.lock();
        let (w, h) = p.get_editor_size().unwrap_or((640, 480));
        let window = EditorWindow::new(&self.name, w.max(100) as u32, h.max(60) as u32)?;
        // SAFETY: the window stays alive until after `close_editor`.
        #[cfg(windows)]
        let handle = unsafe { vst3_host::WindowHandle::from_hwnd(window.raw()) };
        #[cfg(not(windows))]
        let handle = unsafe { vst3_host::WindowHandle::from_raw(window.raw()) };
        p.open_editor(handle).map_err(|e| anyhow!("{e}"))?;
        drop(p);
        self.window = Some(window);
        Ok(())
    }

    pub fn close_editor(&mut self) {
        if self.window.is_some() {
            let _ = self.plugin.lock().close_editor();
            self.window = None;
        }
    }

    pub fn tick(&mut self) {
        if self
            .window
            .as_ref()
            .is_some_and(EditorWindow::close_requested)
        {
            self.close_editor();
        }
        if self.window.is_some() {
            self.plugin.lock().service_run_loop();
        }
    }
}

impl Drop for Vst3Instance {
    fn drop(&mut self) {
        self.close_editor();
        let _ = self.plugin.lock().stop_processing();
    }
}

struct Vst3Fx {
    plugin: Arc<Mutex<Plugin>>,
    buffers: AudioBuffers,
    latency: usize,
}

impl Vst3Fx {
    fn run(&mut self, l: &mut [f32], r: &mut [f32]) {
        // If the UI thread is busy with this plugin (e.g. saving its state), pass audio
        // through for this block rather than waiting on the audio thread.
        let Some(mut p) = self.plugin.try_lock() else {
            return;
        };
        let n = l.len();
        self.buffers.block_size = n;
        self.buffers.inputs[0][..n].copy_from_slice(l);
        self.buffers.inputs[1][..n].copy_from_slice(r);
        if p.process_audio(&mut self.buffers).is_ok() {
            l.copy_from_slice(&self.buffers.outputs[0][..n]);
            r.copy_from_slice(&self.buffers.outputs[1][..n]);
        }
    }
}

impl Effect for Vst3Fx {
    fn set(&mut self, _: &[f32]) {}

    fn process(&mut self, l: &mut f32, r: &mut f32) {
        let (mut a, mut b) = ([*l], [*r]);
        self.run(&mut a, &mut b);
        *l = a[0];
        *r = b[0];
    }

    fn process_block(&mut self, l: &mut [f32], r: &mut [f32]) {
        for (a, b) in l.chunks_mut(MAX_BLOCK).zip(r.chunks_mut(MAX_BLOCK)) {
            self.run(a, b);
        }
    }

    fn reset(&mut self) {}

    fn latency(&self) -> usize {
        self.latency
    }
}
