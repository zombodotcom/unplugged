//! CLAP hosting via [clack](https://github.com/prokopyl/clack) (MIT/Apache).
//!
//! Each plugin instance lives on the UI thread ([`ClapInstance`]); its audio half
//! ([`ClapFx`]) runs inside an effect chain on the audio thread. When the engine lets go
//! of a [`ClapFx`], its processor is sent back to the UI thread so the instance can be
//! deactivated properly.

#![allow(unsafe_code)]

use super::window::EditorWindow;
use super::{InstalledPlugin, MAX_BLOCK, PluginFormat, PluginRef};
use crate::fx::Effect;
use anyhow::{Context, Result, anyhow};
use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, HostGuiImpl, PluginGui, Window as ClapWindow,
};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamRescanFlags,
};
use clack_extensions::state::{HostState, HostStateImpl, PluginState};
use clack_extensions::timer::{HostTimer, HostTimerImpl, PluginTimer, TimerId};
use clack_host::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

enum Msg {
    RunOnMainThread,
    GuiClosed,
    Resize(GuiSize),
}

pub struct Host;

impl HostHandlers for Host {
    type Shared<'a> = Shared;
    type MainThread<'a> = MainThread<'a>;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder
            .register::<HostLog>()
            .register::<HostGui>()
            .register::<HostTimer>()
            .register::<HostParams>()
            .register::<HostState>();
    }
}

pub struct Shared {
    tx: Sender<Msg>,
    dirty: AtomicBool,
}

impl SharedHandler<'_> for Shared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {
        let _ = self.tx.send(Msg::RunOnMainThread);
    }
}

impl HostLogImpl for Shared {
    fn log(&self, severity: LogSeverity, message: &str) {
        if severity > LogSeverity::Info {
            eprintln!("[plugin {severity}] {message}");
        }
    }
}

impl HostGuiImpl for Shared {
    fn resize_hints_changed(&self) {}
    fn request_resize(&self, new_size: GuiSize) -> Result<(), HostError> {
        let _ = self.tx.send(Msg::Resize(new_size));
        Ok(())
    }
    fn request_show(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn request_hide(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn closed(&self, _was_destroyed: bool) {
        let _ = self.tx.send(Msg::GuiClosed);
    }
}

impl HostParamsImplShared for Shared {
    fn request_flush(&self) {}
}

struct Timer {
    id: TimerId,
    period: Duration,
    last: Instant,
}

pub struct MainThread<'a> {
    shared: &'a Shared,
    gui: Cell<Option<PluginGui>>,
    timer_ext: Cell<Option<PluginTimer>>,
    state_ext: Cell<Option<PluginState>>,
    timers: RefCell<Vec<Timer>>,
    next_timer: Cell<u32>,
}

impl<'a> MainThreadHandler<'a> for MainThread<'a> {
    fn initialized(&self, instance: InitializedPluginHandle<'a>) {
        self.gui.set(instance.get_extension());
        self.timer_ext.set(instance.get_extension());
        self.state_ext.set(instance.get_extension());
    }
}

impl HostTimerImpl for MainThread<'_> {
    fn register_timer(&self, period_ms: u32) -> Result<TimerId, HostError> {
        let id = TimerId(self.next_timer.get());
        self.next_timer.set(id.0 + 1);
        self.timers.borrow_mut().push(Timer {
            id,
            period: Duration::from_millis(period_ms.max(15) as u64),
            last: Instant::now(),
        });
        Ok(id)
    }

    fn unregister_timer(&self, timer_id: TimerId) -> Result<(), HostError> {
        self.timers.borrow_mut().retain(|t| t.id != timer_id);
        Ok(())
    }
}

impl HostParamsImplMainThread for MainThread<'_> {
    fn rescan(&self, _flags: ParamRescanFlags) {}
    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

impl HostStateImpl for MainThread<'_> {
    fn mark_dirty(&self) {
        self.shared.dirty.store(true, Ordering::Relaxed);
    }
}

fn host_info() -> HostInfo {
    HostInfo::new(
        "Unplugged",
        "Unplugged",
        "https://github.com/zombodotcom/unplugged",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("valid host info")
}

/// Standard CLAP folders on this computer, plus `CLAP_PATH`.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(p) = std::env::var_os("CLAP_PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    if cfg!(windows) {
        if let Some(c) = std::env::var_os("COMMONPROGRAMFILES") {
            dirs.push(PathBuf::from(c).join("CLAP"));
        }
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(l)
                    .join("Programs")
                    .join("Common")
                    .join("CLAP"),
            );
        }
    } else if cfg!(target_os = "macos") {
        dirs.push("/Library/Audio/Plug-Ins/CLAP".into());
        if let Some(h) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(h).join("Library/Audio/Plug-Ins/CLAP"));
        }
    } else {
        dirs.push("/usr/lib/clap".into());
        if let Some(h) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(h).join(".clap"));
        }
    }
    dirs
}

fn find_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("clap"))
        {
            out.push(p);
        } else if p.is_dir() {
            find_files(&p, out);
        }
    }
}

/// Finds installed CLAP plugins. Loads each file to read its plugin list.
pub fn scan() -> Vec<InstalledPlugin> {
    let mut files = Vec::new();
    for d in search_dirs() {
        find_files(&d, &mut files);
    }
    let mut found = Vec::new();
    for f in files {
        // SAFETY: loading a plugin library runs its code; that's inherent to plugin hosting.
        let Ok(entry) = (unsafe { PluginEntry::load(&f) }) else {
            continue;
        };
        let Some(factory) = entry.get_plugin_factory() else {
            continue;
        };
        for d in factory.plugin_descriptors() {
            let Some(id) = d.id().and_then(|s| s.to_str().ok()) else {
                continue;
            };
            let text = |c: Option<&std::ffi::CStr>| {
                c.map(|c| c.to_string_lossy().to_string())
                    .unwrap_or_default()
            };
            let instrument = d.features().any(|f| f.to_bytes() == b"instrument");
            found.push(InstalledPlugin {
                format: PluginFormat::Clap,
                path: f.to_string_lossy().to_string(),
                id: id.to_string(),
                name: text(d.name()).trim().to_string(),
                vendor: text(d.vendor()),
                instrument,
            });
        }
    }
    found
}

// ---------------------------------------------------------------------------

/// The UI-thread side of one plugin instance.
pub struct ClapInstance {
    instance: PluginInstance<Host>,
    rx: Receiver<Msg>,
    in_ch: usize,
    out_ch: usize,
    sr: f32,
    /// True while the engine holds this instance's audio processor.
    checked_out: bool,
    window: Option<EditorWindow>,
    floating_open: bool,
    pub name: String,
}

pub type Returned = (u64, StoppedPluginAudioProcessor<Host>);

/// Anything that can look up a CLAP instance by slot id.
pub trait InstanceMap {
    fn get_mut(&mut self, slot: u64) -> Option<&mut ClapInstance>;
}

/// Owns loaded plugin files and returns processors coming back from the engine.
pub struct ClapManager {
    entries: HashMap<String, PluginEntry>,
    ret_tx: Sender<Returned>,
    ret_rx: Receiver<Returned>,
}

impl Default for ClapManager {
    fn default() -> Self {
        let (ret_tx, ret_rx) = channel();
        Self {
            entries: HashMap::new(),
            ret_tx,
            ret_rx,
        }
    }
}

impl ClapManager {
    fn entry(&mut self, path: &str) -> Result<PluginEntry> {
        if let Some(e) = self.entries.get(path) {
            return Ok(e.clone());
        }
        // SAFETY: see `scan`.
        let e = unsafe { PluginEntry::load(Path::new(path)) }
            .map_err(|e| anyhow!("can't load {path}: {e}"))?;
        self.entries.insert(path.to_string(), e.clone());
        Ok(e)
    }

    /// Creates an instance and restores `state` into it.
    pub fn instantiate(&mut self, r: &PluginRef, state: &[u8], sr: f32) -> Result<ClapInstance> {
        let entry = self.entry(&r.path)?;
        let id = CString::new(r.id.as_str())?;
        let (tx, rx) = channel();
        let mut instance = PluginInstance::<Host>::new(
            |_| Shared {
                tx,
                dirty: AtomicBool::new(false),
            },
            |shared| MainThread {
                shared,
                gui: Cell::new(None),
                timer_ext: Cell::new(None),
                state_ext: Cell::new(None),
                timers: RefCell::new(Vec::new()),
                next_timer: Cell::new(1),
            },
            &entry,
            &id,
            &host_info(),
        )
        .map_err(|e| anyhow!("{} wouldn't start: {e}", r.name))?;

        if !state.is_empty()
            && let Some(ext) = instance.access_handler(|h| h.state_ext.get())
        {
            let mut reader = state;
            let _ = ext.load(&instance.plugin_handle(), &mut reader);
        }

        let (in_ch, out_ch) = main_ports(&instance.plugin_handle());
        Ok(ClapInstance {
            instance,
            rx,
            in_ch,
            out_ch,
            sr,
            checked_out: false,
            window: None,
            floating_open: false,
            name: r.name.clone(),
        })
    }

    /// Activates `inst` and hands back the effect that runs on the audio thread.
    pub fn activate(
        &mut self,
        slot: u64,
        inst: &mut ClapInstance,
        sr: f32,
    ) -> Result<Box<dyn Effect>> {
        self.drain_returns_into(slot, inst);
        if inst.checked_out {
            return Err(anyhow!("{} is already running", inst.name));
        }
        if inst.instance.is_active() && (inst.sr - sr).abs() > 0.5 {
            let _ = inst.instance.try_deactivate();
        }
        let stopped = inst
            .instance
            .activate(
                |_, _| (),
                PluginAudioConfiguration {
                    sample_rate: sr as f64,
                    min_frames_count: 1,
                    max_frames_count: MAX_BLOCK as u32,
                },
            )
            .map_err(|e| anyhow!("{} couldn't start audio: {e}", inst.name))?;
        let started = stopped
            .start_processing()
            .map_err(|e| anyhow!("{} couldn't start processing: {e:?}", inst.name))?;
        inst.sr = sr;
        inst.checked_out = true;
        Ok(Box::new(ClapFx::new(
            slot,
            started,
            self.ret_tx.clone(),
            inst.in_ch,
            inst.out_ch,
        )))
    }

    /// Takes back processors the engine has dropped and deactivates their instances.
    pub fn drain_returns(&mut self, instances: &mut impl InstanceMap) {
        while let Ok((slot, stopped)) = self.ret_rx.try_recv() {
            if let Some(inst) = instances.get_mut(slot) {
                inst.instance.deactivate(stopped);
                inst.checked_out = false;
            }
            // If the instance is already gone, dropping `stopped` here releases it.
        }
    }

    fn drain_returns_into(&mut self, slot: u64, inst: &mut ClapInstance) {
        let mut others = Vec::new();
        while let Ok((s, stopped)) = self.ret_rx.try_recv() {
            if s == slot {
                inst.instance.deactivate(stopped);
                inst.checked_out = false;
            } else {
                others.push((s, stopped));
            }
        }
        for o in others {
            let _ = self.ret_tx.send(o);
        }
    }
}

fn main_ports(plugin: &PluginMainThreadHandle) -> (usize, usize) {
    let Some(ports) = plugin.get_extension::<PluginAudioPorts>() else {
        return (2, 2);
    };
    let mut buf = AudioPortInfoBuffer::new();
    let mut count = |input: bool| -> usize {
        let n = ports.count(plugin, input);
        if n == 0 {
            return 0;
        }
        // Prefer the port marked "main"; otherwise the first one.
        let mut chosen = None;
        for i in 0..n {
            if let Some(info) = ports.get(plugin, i, input, &mut buf) {
                let ch = info.channel_count as usize;
                if info
                    .flags
                    .contains(clack_extensions::audio_ports::AudioPortFlags::IS_MAIN)
                {
                    return ch.clamp(1, 2);
                }
                chosen.get_or_insert(ch);
            }
        }
        chosen.unwrap_or(2).clamp(1, 2)
    };
    let i = count(true);
    let o = count(false).max(1);
    (i, o)
}

impl ClapInstance {
    pub fn save_state(&mut self) -> Option<Vec<u8>> {
        let ext = self.instance.access_handler(|h| h.state_ext.get())?;
        let mut out = Vec::new();
        ext.save(&self.instance.plugin_handle(), &mut out).ok()?;
        Some(out)
    }

    pub fn take_dirty(&mut self) -> bool {
        self.instance
            .access_shared_handler(|s| s.dirty.swap(false, Ordering::Relaxed))
    }

    pub fn has_editor(&mut self) -> bool {
        self.instance.access_handler(|h| h.gui.get()).is_some()
    }

    pub fn editor_open(&self) -> bool {
        self.window.is_some() || self.floating_open
    }

    pub fn open_editor(&mut self) -> Result<()> {
        if let Some(w) = &self.window {
            w.focus();
            return Ok(());
        }
        let gui = self
            .instance
            .access_handler(|h| h.gui.get())
            .context("this plugin has no window")?;
        let api = GuiApiType::default_for_current_platform()
            .context("plugin windows aren't supported on this OS")?;
        let handle = self.instance.plugin_handle();
        let embedded = GuiConfiguration {
            api_type: api,
            is_floating: false,
        };
        if gui.is_api_supported(&handle, embedded) {
            gui.create(&handle, embedded)
                .map_err(|e| anyhow!("{e:?}"))?;
            let size = gui.get_size(&handle).unwrap_or(GuiSize {
                width: 640,
                height: 480,
            });
            let window = EditorWindow::new(&self.name, size.width, size.height)?;
            // SAFETY: the window outlives the plugin GUI: we destroy the GUI before the window.
            unsafe { gui.set_parent(&handle, ClapWindow::from_win32_hwnd(window.raw())) }
                .map_err(|e| anyhow!("{e:?}"))?;
            let _ = gui.show(&handle);
            self.window = Some(window);
        } else {
            let floating = GuiConfiguration {
                api_type: api,
                is_floating: true,
            };
            if !gui.is_api_supported(&handle, floating) {
                return Err(anyhow!("{} has no window this host can show", self.name));
            }
            gui.create(&handle, floating)
                .map_err(|e| anyhow!("{e:?}"))?;
            if let Ok(title) = CString::new(self.name.as_str()) {
                gui.suggest_title(&handle, &title);
            }
            gui.show(&handle).map_err(|e| anyhow!("{e:?}"))?;
            self.floating_open = true;
        }
        Ok(())
    }

    pub fn close_editor(&mut self) {
        if !self.editor_open() {
            return;
        }
        if let Some(gui) = self.instance.access_handler(|h| h.gui.get()) {
            gui.destroy(&self.instance.plugin_handle());
        }
        self.window = None;
        self.floating_open = false;
    }

    /// Main-thread housekeeping: callbacks, timers, window events.
    pub fn tick(&mut self) {
        let mut close = self
            .window
            .as_ref()
            .is_some_and(EditorWindow::close_requested);
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::RunOnMainThread => self.instance.call_on_main_thread_callback(),
                Msg::GuiClosed => close = true,
                Msg::Resize(s) => {
                    if let Some(w) = &self.window {
                        w.set_client_size(s.width, s.height);
                    }
                }
            }
        }
        let due: Vec<TimerId> = self.instance.access_handler(|h| {
            let now = Instant::now();
            let mut due = Vec::new();
            for t in h.timers.borrow_mut().iter_mut() {
                if now.duration_since(t.last) >= t.period {
                    t.last = now;
                    due.push(t.id);
                }
            }
            due
        });
        if let Some(ext) = self.instance.access_handler(|h| h.timer_ext.get()) {
            for id in due {
                ext.on_timer(&self.instance.plugin_handle(), id);
            }
        }
        if close {
            self.close_editor();
        }
    }
}

impl Drop for ClapInstance {
    fn drop(&mut self) {
        self.close_editor();
    }
}

// ---------------------------------------------------------------------------

/// The audio-thread half: a CLAP plugin inside an effect chain.
pub struct ClapFx {
    slot: u64,
    proc: Option<StartedPluginAudioProcessor<Host>>,
    ret: Sender<Returned>,
    in_ch: usize,
    out_ch: usize,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    inb: Vec<Vec<f32>>,
    outb: Vec<Vec<f32>>,
    steady: u64,
}

impl ClapFx {
    fn new(
        slot: u64,
        proc: StartedPluginAudioProcessor<Host>,
        ret: Sender<Returned>,
        in_ch: usize,
        out_ch: usize,
    ) -> Self {
        Self {
            slot,
            proc: Some(proc),
            ret,
            in_ch,
            out_ch,
            in_ports: AudioPorts::with_capacity(in_ch, 1),
            out_ports: AudioPorts::with_capacity(out_ch, 1),
            inb: vec![vec![0.0; MAX_BLOCK]; in_ch],
            outb: vec![vec![0.0; MAX_BLOCK]; out_ch],
            steady: 0,
        }
    }

    fn run(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        match self.in_ch {
            0 => {}
            1 => {
                for i in 0..n {
                    self.inb[0][i] = (l[i] + r[i]) * 0.5;
                }
            }
            _ => {
                self.inb[0][..n].copy_from_slice(l);
                self.inb[1][..n].copy_from_slice(r);
            }
        }
        let Some(proc) = self.proc.as_mut() else {
            return;
        };
        // Instruments have no input port: the same (filtered-out) port keeps the types equal.
        let has_input = self.in_ch > 0;
        let ins = self.in_ports.with_input_buffers(
            std::iter::once(AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_input_only(self.inb.iter_mut().map(|b| {
                    InputChannel {
                        buffer: &mut b[..n],
                        is_constant: false,
                    }
                })),
            })
            .take(usize::from(has_input)),
        );
        let mut outs = self
            .out_ports
            .with_output_buffers(std::iter::once(AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(
                    self.outb.iter_mut().map(|b| &mut b[..n]),
                ),
            }));
        let ok = proc
            .process(
                &ins,
                &mut outs,
                &InputEvents::empty(),
                &mut OutputEvents::void(),
                Some(self.steady),
                None,
            )
            .is_ok();
        self.steady += n as u64;
        if ok {
            l.copy_from_slice(&self.outb[0][..n]);
            r.copy_from_slice(&self.outb[(self.out_ch - 1).min(1)][..n]);
        }
    }
}

impl Effect for ClapFx {
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
}

impl Drop for ClapFx {
    fn drop(&mut self) {
        if let Some(p) = self.proc.take() {
            let _ = self.ret.send((self.slot, p.stop_processing()));
        }
    }
}
