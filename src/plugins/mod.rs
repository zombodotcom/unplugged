//! Hosting third-party plugins. Both formats are free, open standards:
//! CLAP is MIT-licensed, and the VST3 SDK has been MIT since October 2025.
//!
//! [`PluginHost`] lives on the UI thread and owns every plugin instance, keyed by the
//! effect slot it belongs to. The engine's effect chains ask it (through
//! [`PluginHost::make_effect`]) for the audio-thread half of each plugin.

pub mod catalog;
mod clap;
mod vst3;
mod window;

use crate::fx::{Effect, FxKind, FxSlot};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Largest block we ever ask a plugin to process.
pub const MAX_BLOCK: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PluginFormat {
    Clap,
    Vst3,
}

impl PluginFormat {
    pub fn label(self) -> &'static str {
        match self {
            PluginFormat::Clap => "CLAP",
            PluginFormat::Vst3 => "VST3",
        }
    }
}

/// Which plugin an effect slot uses, and its saved settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginRef {
    pub format: PluginFormat,
    pub path: String,
    /// CLAP plugin id, or VST3 class id (empty = the bundle's first class).
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    /// The plugin's own saved state, stored base64 in project files.
    #[serde(default, with = "b64")]
    pub state: Vec<u8>,
}

mod b64 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

/// A plugin found on this computer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub format: PluginFormat,
    pub path: String,
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub instrument: bool,
}

impl InstalledPlugin {
    pub fn to_ref(&self) -> PluginRef {
        PluginRef {
            format: self.format,
            path: self.path.clone(),
            id: self.id.clone(),
            name: self.name.clone(),
            vendor: self.vendor.clone(),
            state: Vec::new(),
        }
    }
}

/// Scans the standard CLAP and VST3 folders. Loads CLAP files to list them, so run it
/// off the UI thread.
pub fn scan() -> Vec<InstalledPlugin> {
    let mut all = clap::scan();
    all.extend(vst3::scan());
    all.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.format.label().cmp(b.format.label()))
    });
    all
}

fn cache_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Unplugged").join("plugins.json"))
}

/// The last scan's results, so the browser opens instantly.
pub fn load_cache() -> Option<Vec<InstalledPlugin>> {
    let text = std::fs::read_to_string(cache_file()?).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_cache(list: &[InstalledPlugin]) {
    if let Some(f) = cache_file() {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(f, serde_json::to_string(list).unwrap_or_default());
    }
}

enum Instance {
    Clap(clap::ClapInstance),
    Vst3(vst3::Vst3Instance),
}

impl Instance {
    fn new(clap_mgr: &mut clap::ClapManager, r: &PluginRef, state: &[u8], sr: f32) -> Result<Self> {
        Ok(match r.format {
            PluginFormat::Clap => Instance::Clap(clap_mgr.instantiate(r, state, sr)?),
            PluginFormat::Vst3 => Instance::Vst3(vst3::Vst3Instance::new(r, state, sr)?),
        })
    }

    fn save_state(&mut self) -> Option<Vec<u8>> {
        match self {
            Instance::Clap(c) => c.save_state(),
            Instance::Vst3(v) => v.save_state(),
        }
    }
}

/// Owns every running plugin. UI thread only.
pub struct PluginHost {
    sr: f32,
    clap: clap::ClapManager,
    instances: HashMap<u64, Instance>,
    /// Instances for an export mixdown, separate from the live ones.
    offline: Vec<(u64, Instance)>,
    /// Latest known settings of plugins whose slots were removed (so undo brings them back as they were).
    last_state: HashMap<u64, Vec<u8>>,
    /// Slots whose plugin failed to load, with the reason.
    pub failed: HashMap<u64, String>,
    next_offline: u64,
}

impl Default for PluginHost {
    fn default() -> Self {
        Self {
            sr: 48000.0,
            clap: clap::ClapManager::default(),
            instances: HashMap::new(),
            offline: Vec::new(),
            last_state: HashMap::new(),
            failed: HashMap::new(),
            next_offline: u64::MAX / 2,
        }
    }
}

impl PluginHost {
    pub fn set_sample_rate(&mut self, sr: f32) {
        self.sr = sr;
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// The running effect for a plugin slot (creating the plugin if needed).
    pub fn make_effect(&mut self, slot: &FxSlot) -> Option<Box<dyn Effect>> {
        let r = slot.plugin.as_ref()?;
        let sr = self.sr;
        let result = (|| -> Result<Box<dyn Effect>> {
            if !self.instances.contains_key(&slot.id) {
                let state = self
                    .last_state
                    .remove(&slot.id)
                    .unwrap_or_else(|| r.state.clone());
                let inst = Instance::new(&mut self.clap, r, &state, sr)?;
                self.instances.insert(slot.id, inst);
            }
            self.clap.drain_returns(&mut ClapView(&mut self.instances));
            match self.instances.get_mut(&slot.id).unwrap() {
                Instance::Clap(c) => self.clap.activate(slot.id, c, sr),
                Instance::Vst3(v) => v.effect(sr),
            }
        })();
        match result {
            Ok(fx) => {
                self.failed.remove(&slot.id);
                Some(fx)
            }
            Err(e) => {
                self.failed.insert(slot.id, format!("{e:#}"));
                None
            }
        }
    }

    /// A separate instance for rendering a mixdown. Call [`PluginHost::end_offline`] after.
    pub fn make_offline_effect(&mut self, slot: &FxSlot) -> Option<Box<dyn Effect>> {
        let mut r = slot.plugin.clone()?;
        if let Some(live) = self.instances.get_mut(&slot.id)
            && let Some(state) = live.save_state()
        {
            r.state = state;
        }
        let id = self.next_offline;
        self.next_offline += 1;
        let mut inst = Instance::new(&mut self.clap, &r, &r.state, self.sr).ok()?;
        let fx = match &mut inst {
            Instance::Clap(c) => self.clap.activate(id, c, self.sr).ok()?,
            Instance::Vst3(v) => v.effect(self.sr).ok()?,
        };
        self.offline.push((id, inst));
        Some(fx)
    }

    /// Tear down mixdown instances (after their effects have been dropped).
    pub fn end_offline(&mut self) {
        let mut map: HashMap<u64, clap::ClapInstance> = HashMap::new();
        let mut vst = Vec::new();
        for (id, inst) in self.offline.drain(..) {
            match inst {
                Instance::Clap(c) => {
                    map.insert(id, c);
                }
                Instance::Vst3(v) => vst.push(v),
            }
        }
        self.clap.drain_returns(&mut map);
    }

    /// Drop plugins whose slots no longer exist (call right after the engine has synced).
    pub fn retain(&mut self, live: &HashSet<u64>) {
        self.clap.drain_returns(&mut ClapView(&mut self.instances));
        let gone: Vec<u64> = self
            .instances
            .keys()
            .filter(|id| !live.contains(id))
            .copied()
            .collect();
        for id in gone {
            if let Some(mut inst) = self.instances.remove(&id) {
                if let Some(s) = inst.save_state() {
                    self.last_state.insert(id, s);
                }
            }
        }
        self.failed.retain(|id, _| live.contains(id));
    }

    /// Current settings of a running plugin.
    pub fn save_state(&mut self, slot: u64) -> Option<Vec<u8>> {
        self.instances.get_mut(&slot)?.save_state()
    }

    /// True if any plugin reported changed settings since last asked.
    pub fn take_dirty(&mut self) -> bool {
        let mut dirty = false;
        for inst in self.instances.values_mut() {
            if let Instance::Clap(c) = inst {
                dirty |= c.take_dirty();
            }
        }
        dirty
    }

    pub fn is_loaded(&self, slot: u64) -> bool {
        self.instances.contains_key(&slot)
    }

    pub fn has_editor(&mut self, slot: u64) -> bool {
        match self.instances.get_mut(&slot) {
            Some(Instance::Clap(c)) => c.has_editor(),
            Some(Instance::Vst3(v)) => v.has_editor(),
            None => false,
        }
    }

    pub fn editor_open(&self, slot: u64) -> bool {
        match self.instances.get(&slot) {
            Some(Instance::Clap(c)) => c.editor_open(),
            Some(Instance::Vst3(v)) => v.editor_open(),
            None => false,
        }
    }

    pub fn open_editor(&mut self, slot: u64) -> Result<()> {
        match self.instances.get_mut(&slot) {
            Some(Instance::Clap(c)) => c.open_editor(),
            Some(Instance::Vst3(v)) => v.open_editor(),
            None => Err(anyhow!("the plugin isn't loaded")),
        }
    }

    pub fn close_editor(&mut self, slot: u64) {
        match self.instances.get_mut(&slot) {
            Some(Instance::Clap(c)) => c.close_editor(),
            Some(Instance::Vst3(v)) => v.close_editor(),
            None => {}
        }
    }

    /// Per-frame housekeeping on the UI thread.
    pub fn tick(&mut self) {
        self.clap.drain_returns(&mut ClapView(&mut self.instances));
        for inst in self.instances.values_mut() {
            match inst {
                Instance::Clap(c) => c.tick(),
                Instance::Vst3(v) => v.tick(),
            }
        }
    }

    /// Writes every running plugin's current settings into the song's slots.
    pub fn capture_states<'a>(&mut self, slots: impl Iterator<Item = &'a mut FxSlot>) {
        for slot in slots {
            if slot.kind == FxKind::Plugin
                && let Some(state) = self.save_state(slot.id)
                && let Some(r) = slot.plugin.as_mut()
            {
                r.state = state;
            }
        }
    }
}

/// Lets [`clap::ClapManager::drain_returns`] see the CLAP instances inside the shared map.
struct ClapView<'a>(&'a mut HashMap<u64, Instance>);

impl clap::InstanceMap for ClapView<'_> {
    fn get_mut(&mut self, slot: u64) -> Option<&mut clap::ClapInstance> {
        match self.0.get_mut(&slot) {
            Some(Instance::Clap(c)) => Some(c),
            _ => None,
        }
    }
}

impl clap::InstanceMap for HashMap<u64, clap::ClapInstance> {
    fn get_mut(&mut self, slot: u64) -> Option<&mut clap::ClapInstance> {
        HashMap::get_mut(self, &slot)
    }
}
