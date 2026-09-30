//! Command-line social-media exporter. Works on a WAV from any DAW.
//!
//!   unplugged-share song.wav --preset tiktok --title "My Song" --cover art.png --from 42 --to 72
//!   unplugged-share --list

use std::path::PathBuf;
use std::process::ExitCode;
use unplugged::{project, share};

const USAGE: &str = "\
unplugged-share: get a song ready for YouTube, TikTok, Reels, SoundCloud, Discord...

USAGE:
  unplugged-share <input.wav> --preset <id> [options]
  unplugged-share --list

OPTIONS:
  -p, --preset <id>     which platform (see --list)
  -o, --out <file>      output file (default: next to the input, named after the preset)
  -t, --title <text>    title (shown in videos, written to MP3 tags)
  -a, --artist <text>   artist
  -c, --cover <image>   background picture for videos
      --from <sec>      start of a clip, in seconds
      --to <sec>        end of a clip, in seconds
      --lufs <value>    loudness target (default -14)
      --no-normalize    don't change loudness, only make peaks safe
  -h, --help            this help
";

fn main() -> ExitCode {
    match run() {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<String> {
    let mut args = std::env::args().skip(1);
    let mut input: Option<PathBuf> = None;
    let mut preset_id: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut opts = share::ShareOptions::default();
    let (mut from, mut to): (Option<f32>, Option<f32>) = (None, None);

    while let Some(a) = args.next() {
        let mut val = |name: &str| {
            args.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match a.as_str() {
            "-h" | "--help" => return Ok(USAGE.to_string()),
            "--list" => return Ok(list()),
            "-p" | "--preset" => preset_id = Some(val(&a)?),
            "-o" | "--out" => out = Some(val(&a)?.into()),
            "-t" | "--title" => opts.title = val(&a)?,
            "-a" | "--artist" => opts.artist = val(&a)?,
            "-c" | "--cover" => opts.cover = Some(val(&a)?.into()),
            "--from" => from = Some(val(&a)?.parse()?),
            "--to" => to = Some(val(&a)?.parse()?),
            "--lufs" => opts.target_lufs = val(&a)?.parse()?,
            "--no-normalize" => opts.normalize = false,
            s if s.starts_with('-') => anyhow::bail!("unknown option {s}\n\n{USAGE}"),
            s => input = Some(s.into()),
        }
    }

    let input = input.ok_or_else(|| anyhow::anyhow!("no input file\n\n{USAGE}"))?;
    let preset_id = preset_id.ok_or_else(|| anyhow::anyhow!("pick a --preset\n\n{}", list()))?;
    let preset = share::preset(&preset_id)
        .ok_or_else(|| anyhow::anyhow!("unknown preset '{preset_id}'\n\n{}", list()))?;
    if preset.needs_ffmpeg() && share::find_ffmpeg().is_none() {
        anyhow::bail!(share::FFMPEG_HELP);
    }

    let (mut stereo, sr) = project::read_wav_stereo(&input)?;
    if from.is_some() || to.is_some() {
        let frames = stereo.len() / 2;
        let a = from.map_or(0, |s| ((s * sr as f32) as usize).min(frames));
        let b = to.map_or(frames, |s| ((s * sr as f32) as usize).min(frames));
        anyhow::ensure!(b > a, "--to must be after --from");
        stereo = stereo[a * 2..b * 2].to_vec();
        share::fade_edges(&mut stereo, sr, 0.02, 0.5);
    }

    let out = out.unwrap_or_else(|| {
        let stem = input
            .file_stem()
            .map_or("song".into(), |s| s.to_string_lossy().to_string());
        input.with_file_name(format!("{stem}-{}.{}", preset.id, preset.extension()))
    });
    println!("Exporting {} for {}...", input.display(), preset.name);
    Ok(share::export(&stereo, sr, preset, &opts, &out)?.summary())
}

fn list() -> String {
    let mut s = String::from("Presets:\n");
    for p in share::PRESETS {
        s.push_str(&format!("  {:<11} {:<22} {}\n", p.id, p.name, p.hint));
    }
    s
}
