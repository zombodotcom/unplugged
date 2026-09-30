//! "Share" exporter: turn a stereo mix into files ready for YouTube, TikTok, Reels,
//! SoundCloud, Discord, etc.
//!
//! Pipeline: loudness-match (EBU R128) -> true-peak-safe limiter -> encode.
//! WAV / FLAC / MP3 are encoded in-process. Video presets shell out to FFmpeg
//! (kept as a separate program so this project stays MIT and patent-free).

use anyhow::{Context, Result, bail};
use ebur128::{EbuR128, Mode};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    Wav24,
    Flac,
    Mp3 {
        kbps: u32,
    },
    /// H.264 + AAC MP4 with a generated waveform video.
    Video {
        width: u32,
        height: u32,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    /// Where this file is meant to go.
    pub hint: &'static str,
    pub format: Format,
    /// Longest length the platform is happy with, if any (only used for a warning).
    pub max_seconds: Option<f32>,
}

impl Preset {
    pub fn extension(&self) -> &'static str {
        match self.format {
            Format::Wav24 => "wav",
            Format::Flac => "flac",
            Format::Mp3 { .. } => "mp3",
            Format::Video { .. } => "mp4",
        }
    }

    pub fn needs_ffmpeg(&self) -> bool {
        matches!(self.format, Format::Video { .. })
    }
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "youtube",
        name: "YouTube",
        hint: "16:9 video, AAC 384 kbps / 48 kHz (YouTube's recommended upload settings)",
        format: Format::Video {
            width: 1920,
            height: 1080,
        },
        max_seconds: None,
    },
    Preset {
        id: "shorts",
        name: "YouTube Shorts",
        hint: "9:16 vertical video",
        format: Format::Video {
            width: 1080,
            height: 1920,
        },
        max_seconds: Some(180.0),
    },
    Preset {
        id: "tiktok",
        name: "TikTok",
        hint: "9:16 vertical video",
        format: Format::Video {
            width: 1080,
            height: 1920,
        },
        max_seconds: Some(600.0),
    },
    Preset {
        id: "reels",
        name: "Instagram Reels",
        hint: "9:16 vertical video (keep it under 90 s to show in the Reels tab)",
        format: Format::Video {
            width: 1080,
            height: 1920,
        },
        max_seconds: Some(90.0),
    },
    Preset {
        id: "square",
        name: "X / Facebook feed",
        hint: "1:1 square video",
        format: Format::Video {
            width: 1080,
            height: 1080,
        },
        max_seconds: None,
    },
    Preset {
        id: "soundcloud",
        name: "SoundCloud / Bandcamp",
        hint: "Lossless FLAC (CD quality), let the site do the compressing",
        format: Format::Flac,
        max_seconds: None,
    },
    Preset {
        id: "mp3",
        name: "MP3 (share anywhere)",
        hint: "MP3 320 kbps, plays on everything",
        format: Format::Mp3 { kbps: 320 },
        max_seconds: None,
    },
    Preset {
        id: "discord",
        name: "Discord / messages",
        hint: "Small MP3 (128 kbps, ~1 MB per minute) for quick 'listen to this' shares",
        format: Format::Mp3 { kbps: 128 },
        max_seconds: None,
    },
    Preset {
        id: "master",
        name: "Master WAV",
        hint: "24-bit WAV for archiving or sending to a distributor",
        format: Format::Wav24,
        max_seconds: None,
    },
];

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id.eq_ignore_ascii_case(id))
}

#[derive(Clone, Debug)]
pub struct ShareOptions {
    /// Loudness-match to `target_lufs`. When off, only the peak limiter is applied.
    pub normalize: bool,
    pub target_lufs: f32,
    pub title: String,
    pub artist: String,
    /// Background image for video presets.
    pub cover: Option<PathBuf>,
}

impl Default for ShareOptions {
    fn default() -> Self {
        Self {
            normalize: true,
            target_lufs: -14.0,
            title: String::new(),
            artist: String::new(),
            cover: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Report {
    pub out: PathBuf,
    pub seconds: f32,
    pub lufs_before: Option<f64>,
    pub lufs_after: Option<f64>,
    pub true_peak_db: Option<f64>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn summary(&self) -> String {
        let f = |v: Option<f64>| v.map_or("n/a".to_string(), |v| format!("{v:.1}"));
        let mut s = format!(
            "Saved {} ({:.0}s). Loudness {} -> {} LUFS, true peak {} dBTP.",
            self.out.display(),
            self.seconds,
            f(self.lufs_before),
            f(self.lufs_after),
            f(self.true_peak_db)
        );
        for w in &self.warnings {
            s.push_str("\n⚠ ");
            s.push_str(w);
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Loudness + limiting
// ---------------------------------------------------------------------------

/// Integrated loudness (LUFS) and max true peak (dBTP) of interleaved stereo.
pub fn measure(stereo: &[f32], sr: u32) -> (Option<f64>, Option<f64>) {
    let Ok(mut m) = EbuR128::new(2, sr, Mode::I | Mode::TRUE_PEAK) else {
        return (None, None);
    };
    if m.add_frames_f32(stereo).is_err() {
        return (None, None);
    }
    let lufs = m.loudness_global().ok().filter(|v| v.is_finite());
    let tp = (0..2)
        .filter_map(|c| m.true_peak(c).ok())
        .fold(0.0f64, f64::max);
    let tp_db = (tp > 0.0).then(|| 20.0 * tp.log10());
    (lufs, tp_db)
}

/// Offline look-ahead peak limiter, stereo-linked. Never lets a sample exceed `ceiling`.
pub fn limit(stereo: &mut [f32], sr: u32, ceiling: f32) {
    let n = stereo.len() / 2;
    if n == 0 {
        return;
    }
    let look = ((sr as f32 * 0.0015) as usize).max(1);
    let release = 1.0 / (0.08 * sr as f32);

    let need: Vec<f32> = stereo
        .chunks_exact(2)
        .map(|f| {
            let p = f[0].abs().max(f[1].abs());
            if p > ceiling { ceiling / p } else { 1.0 }
        })
        .collect();

    // Sliding minimum over [i - look, i + look] (monotonic deque).
    let mut env = vec![1.0f32; n];
    let mut dq: VecDeque<usize> = VecDeque::new();
    let mut next = 0;
    for (i, e) in env.iter_mut().enumerate() {
        while next < n && next <= i + look {
            while dq.back().is_some_and(|&b| need[b] >= need[next]) {
                dq.pop_back();
            }
            dq.push_back(next);
            next += 1;
        }
        while dq.front().is_some_and(|&f| f + look < i) {
            dq.pop_front();
        }
        *e = need[*dq.front().unwrap()];
    }

    // Smooth release, then a short centred average for click-free attack.
    let mut g = 1.0f32;
    for e in &mut env {
        g = (g + (1.0 - g) * release).min(*e);
        *e = g;
    }
    let half = look / 2;
    let mut smooth = vec![1.0f32; n];
    let mut sum: f32 = 0.0;
    let width = 2 * half + 1;
    let at = |i: isize| {
        env.get(i.clamp(0, n as isize - 1) as usize)
            .copied()
            .unwrap_or(1.0)
    };
    for k in -(half as isize)..=(half as isize) {
        sum += at(k);
    }
    for (i, s) in smooth.iter_mut().enumerate() {
        *s = (sum / width as f32).min(env[i]);
        sum += at(i as isize + half as isize + 1) - at(i as isize - half as isize);
    }

    for (f, g) in stereo.chunks_exact_mut(2).zip(&smooth) {
        f[0] *= g;
        f[1] *= g;
    }
}

/// Loudness-match (optional) and make true-peak safe (≤ -1 dBTP).
/// Returns (loudness before, loudness after, true peak after).
pub fn master(
    stereo: &mut [f32],
    sr: u32,
    normalize: bool,
    target_lufs: f32,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let (before, _) = measure(stereo, sr);
    let original = stereo.to_vec();
    let target = target_lufs as f64;
    // Cap the boost so a near-silent take doesn't become a wall of hiss.
    let first_gain = match (normalize, before) {
        (true, Some(l)) => (target - l).min(24.0),
        _ => 0.0,
    };
    // Peaky material loses loudness in the limiter; allow a little extra make-up gain,
    // but not so much that the guitar gets squashed.
    let max_gain = first_gain + 6.0;
    let mut gain_db = first_gain;
    let mut ceiling_db = -1.5f32;
    let mut result = (None, None);
    for _ in 0..8 {
        let g = 10f32.powf(gain_db as f32 / 20.0);
        for (d, s) in stereo.iter_mut().zip(&original) {
            *d = s * g;
        }
        limit(stereo, sr, 10f32.powf(ceiling_db / 20.0));
        result = measure(stereo, sr);
        let (after, tp) = result;
        if let Some(tp) = tp.filter(|&tp| tp > -1.0) {
            ceiling_db -= (tp as f32 + 1.0) + 0.2;
            continue;
        }
        match after {
            Some(a) if normalize && (target - a).abs() > 0.3 && gain_db < max_gain => {
                gain_db = (gain_db + target - a).min(max_gain);
            }
            _ => break,
        }
    }
    (before, result.0, result.1)
}

/// Short fades so clips don't start or stop with a click.
pub fn fade_edges(stereo: &mut [f32], sr: u32, fade_in_s: f32, fade_out_s: f32) {
    let n = stereo.len() / 2;
    let fin = ((fade_in_s * sr as f32) as usize).min(n);
    let fout = ((fade_out_s * sr as f32) as usize).min(n);
    for i in 0..fin {
        let g = i as f32 / fin as f32;
        stereo[i * 2] *= g;
        stereo[i * 2 + 1] *= g;
    }
    for i in 0..fout {
        let g = i as f32 / fout as f32;
        let j = n - 1 - i;
        stereo[j * 2] *= g;
        stereo[j * 2 + 1] *= g;
    }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Master and encode `stereo` (interleaved) with `preset` into `out`.
pub fn export(
    stereo: &[f32],
    sr: u32,
    preset: &Preset,
    opts: &ShareOptions,
    out: &Path,
) -> Result<Report> {
    if stereo.len() < 2 {
        bail!("Nothing to export - the song is empty.");
    }
    let mut audio = stereo.to_vec();
    let (lufs_before, lufs_after, true_peak_db) =
        master(&mut audio, sr, opts.normalize, opts.target_lufs);
    let seconds = audio.len() as f32 / 2.0 / sr as f32;

    let mut warnings = Vec::new();
    if let Some(max) = preset.max_seconds.filter(|&m| seconds > m) {
        warnings.push(format!(
            "{} prefers {:.0}s or less; this is {:.0}s. Use a clip to trim it.",
            preset.name, max, seconds
        ));
    }
    if lufs_before.is_some_and(|l| l < -40.0) {
        warnings.push("The mix is very quiet. Check your input gain.".into());
    }

    match preset.format {
        Format::Wav24 => crate::project::export_mix(out, &audio, sr)?,
        Format::Flac => write_flac(out, &audio, sr)?,
        Format::Mp3 { kbps } => write_mp3(out, &audio, sr, kbps, opts)?,
        Format::Video { width, height } => write_video(out, &audio, sr, width, height, opts)?,
    }
    Ok(Report {
        out: out.to_path_buf(),
        seconds,
        lufs_before,
        lufs_after,
        true_peak_db,
        warnings,
    })
}

/// 16-bit with TPDF dither. (flacenc 0.5 produces corrupt, huge 24-bit streams on real
/// audio, so FLAC exports are CD quality, which is what SoundCloud/Bandcamp want anyway.)
fn to_i16_dithered(stereo: &[f32]) -> Vec<i32> {
    let mut seed = 0x1234_5678u32;
    let mut rand = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32
    };
    stereo
        .iter()
        .map(|&s| {
            let d = rand() - rand();
            (s.clamp(-1.0, 1.0) * 32_767.0 + d)
                .round()
                .clamp(-32_768.0, 32_767.0) as i32
        })
        .collect()
}

pub fn write_flac(out: &Path, stereo: &[f32], sr: u32) -> Result<()> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;
    let samples = to_i16_dithered(stereo);
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| anyhow::anyhow!("FLAC config: {e:?}"))?;
    let source = flacenc::source::MemSource::from_samples(&samples, 2, 16, sr as usize);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| anyhow::anyhow!("FLAC encode: {e:?}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| anyhow::anyhow!("FLAC write: {e:?}"))?;
    // Guard against encoder bugs: FLAC should never be bigger than raw PCM.
    let raw = samples.len() * 2 + 4096;
    anyhow::ensure!(
        sink.as_slice().len() <= raw,
        "FLAC encoder produced a broken file ({} bytes for {} bytes of audio). Use the Master WAV preset instead.",
        sink.as_slice().len(),
        raw
    );
    std::fs::write(out, sink.as_slice())?;
    Ok(())
}

pub fn write_mp3(
    out: &Path,
    stereo: &[f32],
    sr: u32,
    kbps: u32,
    opts: &ShareOptions,
) -> Result<()> {
    use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, Id3Tag, InterleavedPcm, Quality};
    let brate = match kbps {
        0..=96 => Bitrate::Kbps96,
        97..=128 => Bitrate::Kbps128,
        129..=192 => Bitrate::Kbps192,
        193..=256 => Bitrate::Kbps256,
        _ => Bitrate::Kbps320,
    };
    let e = |e: mp3lame_encoder::BuildError| anyhow::anyhow!("MP3 setup: {e:?}");
    let mut b = Builder::new().context("couldn't create MP3 encoder")?;
    b.set_num_channels(2).map_err(e)?;
    b.set_sample_rate(sr).map_err(e)?;
    b.set_brate(brate).map_err(e)?;
    b.set_quality(Quality::Best).map_err(e)?;
    let _ = b.set_id3_tag(Id3Tag {
        title: opts.title.as_bytes(),
        artist: opts.artist.as_bytes(),
        album: b"",
        album_art: &[],
        year: b"",
        comment: b"Made with Unplugged",
    });
    let mut enc = b.build().map_err(e)?;
    let mut bytes = Vec::with_capacity(stereo.len() / 4);
    for chunk in stereo.chunks(2 * 8192) {
        bytes.reserve(mp3lame_encoder::max_required_buffer_size(chunk.len() / 2));
        enc.encode_to_vec(InterleavedPcm(chunk), &mut bytes)
            .map_err(|e| anyhow::anyhow!("MP3 encode: {e:?}"))?;
    }
    bytes.reserve(7200);
    enc.flush_to_vec::<FlushNoGap>(&mut bytes)
        .map_err(|e| anyhow::anyhow!("MP3 flush: {e:?}"))?;
    std::fs::write(out, bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Video (FFmpeg)
// ---------------------------------------------------------------------------

/// Looks for ffmpeg: $UNPLUGGED_FFMPEG, next to this program, then PATH,
/// then the usual winget install location.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("UNPLUGGED_FFMPEG") {
        candidates.push(p.into());
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join(exe));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join(exe)));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join(exe),
        );
    }
    candidates.into_iter().find(|p| p.is_file())
}

pub const FFMPEG_HELP: &str = "Video export needs FFmpeg (free). Install it with:  winget install Gyan.FFmpeg  \
     (then restart Unplugged), or put ffmpeg.exe next to unplugged.exe.";

/// Escape a path for use inside an FFmpeg filter argument.
fn ff_path(p: &Path) -> String {
    let s = p
        .to_string_lossy()
        .replace('\\', "/")
        .replace(':', "\\:")
        .replace('\'', "\\'");
    format!("'{s}'")
}

fn font_file() -> Option<PathBuf> {
    let candidates = [
        "C:/Windows/Fonts/segoeuib.ttf",
        "C:/Windows/Fonts/arialbd.ttf",
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

pub fn write_video(
    out: &Path,
    stereo: &[f32],
    sr: u32,
    w: u32,
    h: u32,
    opts: &ShareOptions,
) -> Result<()> {
    let ffmpeg = find_ffmpeg().ok_or_else(|| anyhow::anyhow!(FFMPEG_HELP))?;
    let tmp = std::env::temp_dir().join(format!("unplugged-share-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let wav = tmp.join("audio.wav");
    crate::project::write_wav(&wav, stereo, 2, sr)?;

    let mut cmd = Command::new(&ffmpeg);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error"]);
    match &opts.cover {
        Some(img) => {
            cmd.args(["-loop", "1", "-framerate", "30", "-i"]).arg(img);
        }
        None => {
            cmd.args(["-f", "lavfi", "-i"])
                .arg(format!("color=c=0x16161a:s={w}x{h}:r=30"));
        }
    }
    cmd.arg("-i").arg(&wav);

    // Waveform band across the lower part of the frame, title near the top.
    let wave_h = h / 4;
    let wave_y = if h > w { h * 3 / 5 } else { h * 5 / 8 };
    let mut filter = format!(
        "[0:v]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},setsar=1,\
         drawbox=x=0:y={wave_y}:w={w}:h={wave_h}:color=black@0.45:t=fill[bg];\
         [1:a]showwaves=s={w}x{wave_h}:mode=cline:rate=30:scale=sqrt:draw=full:colors=#E8AA50,format=rgba[wv];\
         [bg][wv]overlay=0:{wave_y}:shortest=1[v0]"
    );
    let text = [opts.title.trim(), opts.artist.trim()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>();
    let last = if let (false, Some(font)) = (text.is_empty(), font_file()) {
        let txt = tmp.join("title.txt");
        std::fs::write(&txt, text.join("\n"))?;
        let size = w.min(h) / 16;
        filter.push_str(&format!(
            ";[v0]drawtext=fontfile={}:textfile={}:fontcolor=white:fontsize={size}:line_spacing={}:\
             x=(w-text_w)/2:y=h/6:box=1:boxcolor=black@0.45:boxborderw={}[v]",
            ff_path(&font),
            ff_path(&txt),
            size / 3,
            size / 2
        ));
        "[v]"
    } else {
        "[v0]"
    };

    cmd.args(["-filter_complex", &filter, "-map", last, "-map", "1:a"]);
    cmd.args([
        "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p", "-r", "30",
    ]);
    cmd.args([
        "-c:a",
        "aac",
        "-b:a",
        "384k",
        "-ar",
        "48000",
        "-movflags",
        "+faststart",
        "-shortest",
    ]);
    cmd.arg(out);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = cmd.output().context("couldn't run ffmpeg")?;
    let _ = std::fs::remove_dir_all(&tmp);
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        bail!(
            "FFmpeg failed: {}",
            err.lines().rev().take(4).collect::<Vec<_>>().join(" | ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn tone(sr: u32, secs: f32, amp: f32) -> Vec<f32> {
        (0..(sr as f32 * secs) as usize)
            .flat_map(|i| {
                let s = (2.0 * PI * 220.0 * i as f32 / sr as f32).sin() * amp;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn master_hits_target_and_peak_ceiling() {
        let sr = 48000;
        let mut a = tone(sr, 5.0, 0.05);
        let (before, after, tp) = master(&mut a, sr, true, -14.0);
        assert!(before.unwrap() < -25.0);
        assert!((after.unwrap() + 14.0).abs() < 1.0, "after {after:?}");
        assert!(tp.unwrap() <= -0.9, "tp {tp:?}");
    }

    #[test]
    fn limiter_catches_spikes() {
        let sr = 48000;
        let mut a = tone(sr, 1.0, 0.3);
        a[20000] = 1.8;
        a[20001] = -1.8;
        limit(&mut a, sr, 0.8);
        assert!(a.iter().all(|s| s.abs() <= 0.8 + 1e-4));
        // Quiet parts far from the spike are untouched.
        assert!((a[2 * 1000] - tone(sr, 1.0, 0.3)[2 * 1000]).abs() < 1e-6);
    }

    #[test]
    fn encodes_flac_and_mp3() {
        let sr = 48000;
        let a = tone(sr, 1.0, 0.5);
        let dir = std::env::temp_dir().join(format!("unplugged-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for id in ["soundcloud", "discord", "master"] {
            let p = preset(id).unwrap();
            let out = dir.join(format!("t.{}", p.extension()));
            let r = export(&a, sr, p, &ShareOptions::default(), &out).unwrap();
            let len = std::fs::metadata(&out).unwrap().len();
            assert!(len > 1000, "{id} wrote {len} bytes");
            assert!(
                len < 48000 * 2 * 4,
                "{id} wrote {len} bytes, more than raw audio"
            );
            assert!(r.lufs_after.is_some());
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
