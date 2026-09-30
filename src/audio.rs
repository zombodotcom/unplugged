//! Audio device handling (cpal). Input and output run as two streams; the input
//! callback pushes the chosen mono channel into a queue that the output callback drains.

use crate::engine::Engine;
use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, FromSample, HostId, SampleFormat, SizedSample, StreamConfig};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

pub type InputQueue = Arc<Mutex<VecDeque<f32>>>;
pub type ErrorSlot = Arc<Mutex<Option<String>>>;

pub struct DeviceInfo {
    pub name: String,
    pub device: Device,
    pub channels: u16,
}

pub fn hosts() -> Vec<HostId> {
    cpal::available_hosts()
}

pub fn list_devices(host: HostId) -> (Vec<DeviceInfo>, Vec<DeviceInfo>) {
    let Ok(host) = cpal::host_from_id(host) else {
        return (vec![], vec![]);
    };
    let collect = |devs: Vec<Device>, input: bool| {
        devs.into_iter()
            .filter_map(|d| {
                let cfg = if input {
                    d.default_input_config()
                } else {
                    d.default_output_config()
                }
                .ok()?;
                Some(DeviceInfo {
                    name: d.to_string(),
                    channels: cfg.channels(),
                    device: d,
                })
            })
            .collect::<Vec<_>>()
    };
    let inputs = host
        .input_devices()
        .map(|d| d.collect())
        .unwrap_or_default();
    let outputs = host
        .output_devices()
        .map(|d| d.collect())
        .unwrap_or_default();
    (collect(inputs, true), collect(outputs, false))
}

/// Prefer a Focusrite/Scarlett device if there is one.
pub fn pick_scarlett(devs: &[DeviceInfo]) -> Option<usize> {
    devs.iter().position(|d| {
        let n = d.name.to_lowercase();
        n.contains("scarlett") || n.contains("focusrite")
    })
}

/// Index of the host's default input/output device in the lists.
pub fn default_indices(
    host: HostId,
    inputs: &[DeviceInfo],
    outputs: &[DeviceInfo],
) -> (usize, usize) {
    let Ok(host) = cpal::host_from_id(host) else {
        return (0, 0);
    };
    let find = |devs: &[DeviceInfo], d: Option<Device>| {
        d.and_then(|d| devs.iter().position(|x| x.name == d.to_string()))
            .unwrap_or(0)
    };
    (
        find(inputs, host.default_input_device()),
        find(outputs, host.default_output_device()),
    )
}

/// On a Scarlett Solo the guitar (instrument) jack is input 2.
pub fn default_input_channel(name: &str) -> u16 {
    if name.to_lowercase().contains("solo") {
        1
    } else {
        0
    }
}

/// The devices the user last ran with, remembered between launches.
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct SavedDevices {
    pub host: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub channel: Option<u16>,
    pub buffer: Option<u32>,
}

impl SavedDevices {
    fn path() -> Option<std::path::PathBuf> {
        std::env::var_os("APPDATA").map(|a| {
            std::path::PathBuf::from(a)
                .join("Unplugged")
                .join("devices.json")
        })
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = Self::path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, serde_json::to_string_pretty(self).unwrap_or_default());
        }
    }

    pub fn host(&self) -> Option<HostId> {
        let name = self.host.as_deref()?;
        hosts().into_iter().find(|h| h.name() == name)
    }
}

pub struct AudioSettings {
    pub host: HostId,
    pub input: Device,
    pub output: Device,
    /// Zero-based input channel carrying the guitar.
    pub input_channel: u16,
    /// Requested buffer size in frames, or None for the driver default.
    pub buffer: Option<u32>,
}

pub struct RunningAudio {
    _input: cpal::Stream,
    _output: cpal::Stream,
    pub description: String,
}

pub fn start(
    s: &AudioSettings,
    engine: Arc<Mutex<Engine>>,
    queue: InputQueue,
    errors: ErrorSlot,
    repaint: impl Fn() + Send + Clone + 'static,
) -> Result<RunningAudio> {
    let out_default = s
        .output
        .default_output_config()
        .context("output device has no default config")?;
    let in_default = s
        .input
        .default_input_config()
        .context("input device has no default config")?;
    // Windows often has a device's input and output at different rates (e.g. 48k in, 44.1k out).
    // Try to run both at the same rate; if the driver won't, resample the input.
    let (out_cfg, in_cfg) = if in_default.sample_rate() == out_default.sample_rate() {
        (out_default, in_default)
    } else if let Some(o) = s
        .output
        .supported_output_configs()?
        .filter(|c| {
            c.sample_format() == out_default.sample_format()
                && c.channels() == out_default.channels()
        })
        .find_map(|c| c.try_with_sample_rate(in_default.sample_rate()))
    {
        (o, in_default)
    } else if let Some(i) = s
        .input
        .supported_input_configs()?
        .filter(|c| {
            c.sample_format() == in_default.sample_format() && c.channels() == in_default.channels()
        })
        .find_map(|c| c.try_with_sample_rate(out_default.sample_rate()))
    {
        (out_default, i)
    } else {
        (out_default, in_default)
    };
    let sr = out_cfg.sample_rate();
    let in_sr = in_cfg.sample_rate();
    let in_channels = in_cfg.channels();
    if s.input_channel >= in_channels {
        return Err(anyhow!(
            "Input channel {} doesn't exist (device has {in_channels})",
            s.input_channel + 1
        ));
    }

    engine.lock().set_sample_rate(sr as f32);
    queue.lock().clear();

    let buffer = s.buffer.map_or(BufferSize::Default, BufferSize::Fixed);
    let in_stream_cfg = StreamConfig {
        channels: in_channels,
        sample_rate: in_sr,
        buffer_size: buffer,
    };
    let out_stream_cfg = StreamConfig {
        channels: out_cfg.channels(),
        sample_rate: sr,
        buffer_size: buffer,
    };

    let input = match in_cfg.sample_format() {
        SampleFormat::F32 => {
            build_input::<f32>(s, &in_stream_cfg, sr, queue.clone(), errors.clone())
        }
        SampleFormat::I16 => {
            build_input::<i16>(s, &in_stream_cfg, sr, queue.clone(), errors.clone())
        }
        SampleFormat::I32 => {
            build_input::<i32>(s, &in_stream_cfg, sr, queue.clone(), errors.clone())
        }
        f => Err(anyhow!("unsupported input sample format {f}")),
    }?;
    let output = match out_cfg.sample_format() {
        SampleFormat::F32 => {
            build_output::<f32>(s, &out_stream_cfg, engine, queue, errors, repaint)
        }
        SampleFormat::I16 => {
            build_output::<i16>(s, &out_stream_cfg, engine, queue, errors, repaint)
        }
        SampleFormat::I32 => {
            build_output::<i32>(s, &out_stream_cfg, engine, queue, errors, repaint)
        }
        f => Err(anyhow!("unsupported output sample format {f}")),
    }?;
    input.play()?;
    output.play()?;

    let buf = s.buffer.map_or("default buffer".to_string(), |b| {
        format!("{b} frames ({:.1} ms)", b as f32 / sr as f32 * 1000.0)
    });
    Ok(RunningAudio {
        _input: input,
        _output: output,
        description: if in_sr == sr {
            format!("{} · {} Hz · {}", s.host.name(), sr, buf)
        } else {
            format!(
                "{} · in {} Hz resampled to {} Hz · {}",
                s.host.name(),
                in_sr,
                sr,
                buf
            )
        },
    })
}

fn build_input<T>(
    s: &AudioSettings,
    cfg: &StreamConfig,
    out_sr: u32,
    queue: InputQueue,
    errors: ErrorSlot,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = cfg.channels as usize;
    let ch = s.input_channel as usize;
    // Never let more than half a second pile up (e.g. while the output is stalled).
    let max_queue = out_sr as usize / 2;
    let mut resampler = LinearResampler::new(cfg.sample_rate, out_sr);
    let started = std::time::Instant::now();
    let stream = s.input.build_input_stream::<T, _, _>(
        *cfg,
        move |data: &[T], _| {
            let mut q = queue.lock();
            for f in data.chunks_exact(channels) {
                resampler.push(f[ch].to_sample::<f32>(), |y| q.push_back(y));
            }
            let excess = q.len().saturating_sub(max_queue);
            q.drain(..excess);
        },
        move |e| report(&errors, "Input", &e, started),
        None,
    )?;
    Ok(stream)
}

fn build_output<T>(
    s: &AudioSettings,
    cfg: &StreamConfig,
    engine: Arc<Mutex<Engine>>,
    queue: InputQueue,
    errors: ErrorSlot,
    repaint: impl Fn() + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = cfg.channels as usize;
    let mut stereo: Vec<f32> = Vec::with_capacity(8192);
    let mut input: VecDeque<f32> = VecDeque::with_capacity(8192);
    let mut calls = 0u32;
    let started = std::time::Instant::now();
    let stream = s.output.build_output_stream::<T, _, _>(
        *cfg,
        move |data: &mut [T], _| {
            let frames = data.len() / channels;
            {
                // Keep monitoring latency low: don't let the input backlog exceed ~2 callbacks.
                let mut q = queue.lock();
                let excess = q.len().saturating_sub(frames * 2);
                q.drain(..excess);
                let take = q.len().min(frames);
                input.extend(q.drain(..take));
            }
            stereo.clear();
            stereo.resize(frames * 2, 0.0);
            if let Some(mut e) = engine.try_lock() {
                e.render(&mut input, &mut stereo);
            }
            input.clear();
            for (out, st) in data.chunks_exact_mut(channels).zip(stereo.chunks_exact(2)) {
                if channels == 1 {
                    out[0] = T::from_sample((st[0] + st[1]) * 0.5);
                } else {
                    out[0] = T::from_sample(st[0]);
                    out[1] = T::from_sample(st[1]);
                    for o in &mut out[2..] {
                        *o = T::EQUILIBRIUM;
                    }
                }
            }
            calls = calls.wrapping_add(1);
            if calls.is_multiple_of(4) {
                repaint();
            }
        },
        move |e| report(&errors, "Output", &e, started),
        None,
    )?;
    Ok(stream)
}

fn report(errors: &ErrorSlot, side: &str, e: &cpal::Error, started: std::time::Instant) {
    let msg = e.to_string();
    let xrun = msg.to_lowercase().contains("underrun") || msg.to_lowercase().contains("overrun");
    let text = if !xrun {
        format!("{side} error: {msg}")
    } else if started.elapsed().as_secs_f32() > 2.0 {
        // A glitch or two while streams spin up is normal; only report later ones.
        format!(
            "{side} glitch (buffer xrun). If you hear crackles, pick a bigger buffer in Audio settings."
        )
    } else {
        return;
    };
    *errors.lock() = Some(text);
}

/// Streaming linear-interpolation resampler (pass-through when rates match).
struct LinearResampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl LinearResampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: 0.0,
        }
    }

    #[inline]
    fn push(&mut self, x: f32, mut out: impl FnMut(f32)) {
        if self.step == 1.0 {
            return out(x);
        }
        while self.pos < 1.0 {
            out(self.prev + (x - self.prev) * self.pos as f32);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.prev = x;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_ratio() {
        let mut r = LinearResampler::new(48000, 44100);
        let mut n = 0;
        for i in 0..48000 {
            r.push((i as f32 * 0.01).sin(), |_| n += 1);
        }
        assert!((n as i32 - 44100).abs() <= 1, "{n}");
    }
}

/// Real-hardware check: `cargo test --release -- --ignored realtek --nocapture`
#[cfg(test)]
mod hw_tests {
    use super::*;

    #[test]
    #[ignore]
    fn scarlett_in_realtek_out() {
        let host = cpal::default_host().id();
        let (ins, outs) = list_devices(host);
        let input = &ins[pick_scarlett(&ins).expect("no scarlett")];
        for out in outs.iter().filter(|d| d.name.contains("Realtek")) {
            let engine = Arc::new(Mutex::new(Engine::new(48000.0)));
            {
                let mut e = engine.lock();
                e.monitor = false; // silent
                e.play();
            }
            let errors: ErrorSlot = Default::default();
            let s = AudioSettings {
                host,
                input: input.device.clone(),
                output: out.device.clone(),
                input_channel: 1,
                buffer: None,
            };
            match start(
                &s,
                engine.clone(),
                Default::default(),
                errors.clone(),
                || {},
            ) {
                Ok(a) => {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    let e = engine.lock();
                    println!(
                        "{} -> OK: {} | frames rendered {} | err {:?}",
                        out.name,
                        a.description,
                        e.playhead,
                        errors.lock()
                    );
                }
                Err(e) => println!("{} -> FAILED: {e:#}", out.name),
            }
        }
    }
}
