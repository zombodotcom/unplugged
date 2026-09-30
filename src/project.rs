//! Saving/loading projects and WAV import/export.
//!
//! A project is a folder containing `project.json` plus one 32-bit float WAV per track.

use crate::dsp::{MAX_IR_SECONDS, SimParams, normalize_energy};
use crate::engine::{TrackView, compute_peaks};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
pub struct ProjectFile {
    pub sample_rate: u32,
    pub bpm: f32,
    pub sim: SimParams,
    pub tracks: Vec<TrackEntry>,
}

#[derive(Serialize, Deserialize)]
pub struct TrackEntry {
    pub name: String,
    pub file: String,
    pub start: usize,
    pub volume: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub acoustic: bool,
}

pub fn save(dir: &Path, sr: u32, bpm: f32, sim: &SimParams, tracks: &[TrackView]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut entries = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        let file = format!("track{:02}.wav", i + 1);
        write_wav(&dir.join(&file), &t.samples, 1, sr)?;
        entries.push(TrackEntry {
            name: t.name.clone(),
            file,
            start: t.start,
            volume: t.volume,
            pan: t.pan,
            mute: t.mute,
            solo: t.solo,
            acoustic: t.acoustic,
        });
    }
    let pf = ProjectFile {
        sample_rate: sr,
        bpm,
        sim: sim.clone(),
        tracks: entries,
    };
    std::fs::write(dir.join("project.json"), serde_json::to_string_pretty(&pf)?)?;
    Ok(())
}

/// Loads a project, resampling tracks to `sr` if needed.
pub fn load(dir: &Path, sr: u32) -> Result<(ProjectFile, Vec<TrackView>)> {
    let text = std::fs::read_to_string(dir.join("project.json"))
        .context("no project.json in that folder")?;
    let pf: ProjectFile = serde_json::from_str(&text)?;
    let ratio = sr as f64 / pf.sample_rate as f64;
    let mut views = Vec::new();
    for e in &pf.tracks {
        let samples = read_wav_mono(&dir.join(&e.file), sr)?;
        views.push(track_view(
            e.name.clone(),
            samples,
            (e.start as f64 * ratio) as usize,
            e.volume,
            e.pan,
            e.mute,
            e.solo,
            e.acoustic,
        ));
    }
    Ok((pf, views))
}

#[allow(clippy::too_many_arguments)]
pub fn track_view(
    name: String,
    samples: Vec<f32>,
    start: usize,
    volume: f32,
    pan: f32,
    mute: bool,
    solo: bool,
    acoustic: bool,
) -> TrackView {
    TrackView {
        id: 0,
        name,
        peaks: Arc::new(compute_peaks(&samples)),
        samples: Arc::new(samples),
        start,
        volume,
        pan,
        mute,
        solo,
        acoustic,
    }
}

pub fn write_wav(path: &Path, samples: &[f32], channels: u16, sr: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels,
        sample_rate: sr,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        w.write_sample(s)?;
    }
    w.finalize()?;
    Ok(())
}

/// Writes 24-bit stereo, the most widely compatible "good quality" export.
pub fn export_mix(path: &Path, stereo: &[f32], sr: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: sr,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in stereo {
        w.write_sample((s.clamp(-1.0, 1.0) * 8_388_607.0) as i32)?;
    }
    w.finalize()?;
    Ok(())
}

/// Reads a WAV as interleaved f32. Returns (samples, channels, sample rate).
fn read_wav(path: &Path) -> Result<(Vec<f32>, usize, u32)> {
    let mut r =
        hound::WavReader::open(path).with_context(|| format!("can't open {}", path.display()))?;
    let spec = r.spec();
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()?
        }
    };
    Ok((interleaved, spec.channels as usize, spec.sample_rate))
}

/// Reads any WAV as mono f32 (channels averaged), resampled to `sr`.
pub fn read_wav_mono(path: &Path, sr: u32) -> Result<Vec<f32>> {
    let (interleaved, ch, file_sr) = read_wav(path)?;
    let mono: Vec<f32> = interleaved
        .chunks(ch)
        .map(|f| f.iter().sum::<f32>() / ch as f32)
        .collect();
    Ok(resample(&mono, file_sr, sr))
}

/// Reads any WAV as interleaved stereo at its own sample rate (mono is duplicated,
/// extra channels are dropped). Returns (samples, sample rate).
pub fn read_wav_stereo(path: &Path) -> Result<(Vec<f32>, u32)> {
    let (interleaved, ch, sr) = read_wav(path)?;
    let stereo = interleaved
        .chunks(ch)
        .flat_map(|f| [f[0], if ch > 1 { f[1] } else { f[0] }])
        .collect();
    Ok((stereo, sr))
}

/// Linear-interpolation resampler. Good enough for IRs and occasional imports.
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let step = from as f64 / to as f64;
    let n = (x.len() as f64 / step) as usize;
    (0..n)
        .map(|i| {
            let p = i as f64 * step;
            let i0 = p as usize;
            let f = (p - i0 as f64) as f32;
            let a = x[i0];
            let b = x.get(i0 + 1).copied().unwrap_or(a);
            a + (b - a) * f
        })
        .collect()
}

/// Loads an acoustic-sim IR, trimmed to [`MAX_IR_SECONDS`] and loudness-normalised.
pub fn load_ir(path: &Path, sr: u32) -> Result<Vec<f32>> {
    let mut ir = read_wav_mono(path, sr)?;
    ir.truncate((MAX_IR_SECONDS * sr as f32) as usize);
    anyhow::ensure!(!ir.is_empty(), "IR file is empty");
    normalize_energy(&mut ir);
    Ok(ir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("unplugged-test-{}", std::process::id()));
        let t = track_view(
            "Rhythm".into(),
            vec![0.1, -0.2, 0.3],
            480,
            0.7,
            -0.5,
            false,
            true,
            true,
        );
        save(&dir, 48000, 100.0, &SimParams::default(), &[t]).unwrap();
        let (pf, views) = load(&dir, 48000).unwrap();
        assert_eq!(pf.bpm, 100.0);
        assert_eq!(views[0].name, "Rhythm");
        assert_eq!(views[0].start, 480);
        assert_eq!(*views[0].samples, vec![0.1, -0.2, 0.3]);
        assert!(views[0].solo && views[0].acoustic);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn resample_halves_length() {
        let x: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let y = resample(&x, 96000, 48000);
        assert_eq!(y.len(), 50);
        assert_eq!(y[10], 20.0);
    }
}
