//! Saving/loading projects and WAV import/export.
//!
//! A project is a folder containing `project.json` plus the recorded audio as 32-bit
//! float WAVs in `audio/`. Clips reference a window of one of those files, so splits
//! and duplicates don't copy audio on disk either.

use crate::dsp::{MAX_IR_SECONDS, SimParams, normalize_energy};
use crate::model::{Clip, Doc, Track, compute_peaks};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub const FORMAT_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
pub struct ProjectFile {
    #[serde(default)]
    pub version: u32,
    pub sample_rate: u32,
    pub bpm: f32,
    pub sim: SimParams,
    pub tracks: Vec<TrackEntry>,
}

#[derive(Serialize, Deserialize)]
pub struct TrackEntry {
    pub name: String,
    pub volume: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub acoustic: bool,
    #[serde(default)]
    pub invert: bool,
    #[serde(default)]
    pub clips: Vec<ClipEntry>,
    /// v1 projects: one file per track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<usize>,
}

#[derive(Serialize, Deserialize)]
pub struct ClipEntry {
    pub file: String,
    pub start: usize,
    pub offset: usize,
    pub len: usize,
}

/// Audio files already written to a folder, so repeated saves only write new audio.
/// Holds the audio alive so its address can't be reused by a different recording.
#[derive(Default)]
pub struct WrittenAudio {
    files: HashMap<usize, (Arc<Vec<f32>>, String)>,
    prefix: String,
}

impl WrittenAudio {
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self {
            files: HashMap::new(),
            prefix: prefix.into(),
        }
    }
}

pub fn save(dir: &Path, sr: u32, bpm: f32, sim: &SimParams, tracks: &[Track]) -> Result<()> {
    save_incremental(
        dir,
        sr,
        bpm,
        sim,
        tracks,
        &mut WrittenAudio::with_prefix("source"),
    )
}

/// Like [`save`] but skips audio already in `written` (used by autosave).
pub fn save_incremental(
    dir: &Path,
    sr: u32,
    bpm: f32,
    sim: &SimParams,
    tracks: &[Track],
    written: &mut WrittenAudio,
) -> Result<()> {
    let audio = dir.join("audio");
    std::fs::create_dir_all(&audio)?;
    // One file per distinct recording, however many clips use it.
    let mut entries = Vec::new();
    for t in tracks {
        let mut clips = Vec::new();
        for c in &t.clips {
            let key = Arc::as_ptr(&c.source) as usize;
            let file = match written.files.get(&key) {
                Some((_, f)) => f.clone(),
                None => {
                    let f = format!("audio/{}{:03}.wav", written.prefix, written.files.len() + 1);
                    write_wav(&dir.join(&f), &c.source, 1, sr)?;
                    written.files.insert(key, (c.source.clone(), f.clone()));
                    f
                }
            };
            clips.push(ClipEntry {
                file,
                start: c.start,
                offset: c.offset,
                len: c.len,
            });
        }
        entries.push(TrackEntry {
            name: t.name.clone(),
            volume: t.volume,
            pan: t.pan,
            mute: t.mute,
            solo: t.solo,
            acoustic: t.acoustic,
            invert: t.invert,
            clips,
            file: None,
            start: None,
        });
    }
    let pf = ProjectFile {
        version: FORMAT_VERSION,
        sample_rate: sr,
        bpm,
        sim: sim.clone(),
        tracks: entries,
    };
    // Write then rename, so a crash mid-save never leaves a half-written project.json.
    let tmp = dir.join("project.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&pf)?)?;
    std::fs::rename(&tmp, dir.join("project.json"))?;
    Ok(())
}

/// Where the background autosave goes. Removed on a clean exit, so if it's still
/// there at startup, the last session crashed.
pub fn autosave_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA").map(|a| {
        std::path::PathBuf::from(a)
            .join("Unplugged")
            .join("autosave")
    })
}

pub fn has_crash_backup() -> bool {
    autosave_dir().is_some_and(|d| d.join("project.json").is_file())
}

pub fn clear_autosave() {
    if let Some(d) = autosave_dir() {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Loads a project (v1 or v2), resampling audio to `sr` if needed. Ids come from `doc`.
pub fn load(dir: &Path, sr: u32, doc: &mut Doc) -> Result<(ProjectFile, Vec<Track>)> {
    let text = std::fs::read_to_string(dir.join("project.json"))
        .context("no project.json in that folder")?;
    let pf: ProjectFile = serde_json::from_str(&text)?;
    let ratio = sr as f64 / pf.sample_rate as f64;
    let scale = |v: usize| (v as f64 * ratio).round() as usize;
    let mut sources: HashMap<String, (Arc<Vec<f32>>, Arc<Vec<[f32; 2]>>)> = HashMap::new();
    let mut load_source = |file: &str| -> Result<(Arc<Vec<f32>>, Arc<Vec<[f32; 2]>>)> {
        if let Some(s) = sources.get(file) {
            return Ok(s.clone());
        }
        let samples = read_wav_mono(&dir.join(file), sr)?;
        let peaks = Arc::new(compute_peaks(&samples));
        let s = (Arc::new(samples), peaks);
        sources.insert(file.to_string(), s.clone());
        Ok(s)
    };

    let mut tracks = Vec::new();
    for e in &pf.tracks {
        let mut clips = Vec::new();
        let entries: Vec<ClipEntry> = match (&e.file, e.start) {
            // v1: the whole file is one clip.
            (Some(f), start) if e.clips.is_empty() => vec![ClipEntry {
                file: f.clone(),
                start: start.unwrap_or(0),
                offset: 0,
                len: usize::MAX,
            }],
            _ => e
                .clips
                .iter()
                .map(|c| ClipEntry {
                    file: c.file.clone(),
                    start: c.start,
                    offset: c.offset,
                    len: c.len,
                })
                .collect(),
        };
        for c in entries {
            let (source, peaks) = load_source(&c.file)?;
            let offset = scale(c.offset).min(source.len());
            let len = if c.len == usize::MAX {
                source.len()
            } else {
                scale(c.len)
            }
            .min(source.len() - offset);
            if len == 0 {
                continue;
            }
            clips.push(Clip {
                id: doc.new_id(),
                source,
                peaks,
                start: scale(c.start),
                offset,
                len,
            });
        }
        tracks.push(Track {
            id: doc.new_id(),
            name: e.name.clone(),
            clips,
            volume: e.volume,
            pan: e.pan,
            mute: e.mute,
            solo: e.solo,
            acoustic: e.acoustic,
            invert: e.invert,
        });
    }
    Ok((pf, tracks))
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
        let mut d = Doc::new();
        let clip = d.make_clip((0..1000).map(|i| i as f32 / 1000.0).collect(), 480);
        let cid = clip.id;
        let tid = d.add_track("Rhythm".into(), Some(clip), true);
        d.track_mut(tid).unwrap().solo = true;
        let right = d.split(cid, 980).unwrap();
        d.duplicate(crate::model::Selection::Clip(right));
        save(&dir, 48000, 100.0, &SimParams::default(), &d.tracks).unwrap();
        // Split + duplicate share one recording, so only one audio file is written.
        assert_eq!(std::fs::read_dir(dir.join("audio")).unwrap().count(), 1);

        let mut d2 = Doc::new();
        let (pf, tracks) = load(&dir, 48000, &mut d2).unwrap();
        assert_eq!(pf.bpm, 100.0);
        assert_eq!(tracks[0].name, "Rhythm");
        assert!(tracks[0].solo && tracks[0].acoustic);
        assert!(!tracks[0].invert);
        assert_eq!(tracks[0].clips.len(), 3);
        for pos in [480, 979, 980, 1479, 1480, 1979] {
            assert_eq!(
                tracks[0].sample_at(pos),
                d.tracks[0].sample_at(pos),
                "pos {pos}"
            );
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn incremental_save_only_writes_new_audio() {
        let dir = std::env::temp_dir().join(format!("unplugged-inc-{}", std::process::id()));
        let mut d = Doc::new();
        let c = d.make_clip(vec![0.1; 500], 0);
        d.add_track("a".into(), Some(c), false);
        let mut w = WrittenAudio::with_prefix("auto");
        save_incremental(&dir, 48000, 90.0, &SimParams::default(), &d.tracks, &mut w).unwrap();
        let first = std::fs::metadata(dir.join("audio/auto001.wav"))
            .unwrap()
            .modified()
            .unwrap();
        let c2 = d.make_clip(vec![0.2; 500], 600);
        d.add_track("b".into(), Some(c2), false);
        std::thread::sleep(std::time::Duration::from_millis(20));
        save_incremental(&dir, 48000, 90.0, &SimParams::default(), &d.tracks, &mut w).unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("audio/auto001.wav"))
                .unwrap()
                .modified()
                .unwrap(),
            first
        );
        assert!(dir.join("audio/auto002.wav").is_file());
        let (_, tracks) = load(&dir, 48000, &mut Doc::new()).unwrap();
        assert_eq!(tracks.len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn loads_v1_projects() {
        let dir = std::env::temp_dir().join(format!("unplugged-v1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_wav(&dir.join("track01.wav"), &[0.5; 100], 1, 48000).unwrap();
        std::fs::write(
            dir.join("project.json"),
            r#"{"sample_rate":48000,"bpm":90,"sim":{},"tracks":[{"name":"Take 1","file":"track01.wav","start":10,"volume":0.8,"pan":0,"mute":false,"solo":false,"acoustic":true}]}"#,
        )
        .unwrap();
        let (_, tracks) = load(&dir, 48000, &mut Doc::new()).unwrap();
        assert_eq!(tracks[0].clips.len(), 1);
        assert_eq!(
            (tracks[0].clips[0].start, tracks[0].clips[0].len),
            (10, 100)
        );
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
