# Unplugged

A free, open-source DAW (digital audio workstation) for recording at home: plug a guitar, mic or anything else into your audio interface, record takes, layer them, edit them, add effects, and share the result to YouTube, TikTok, Reels or SoundCloud in one click.

It started as a way to play an electric guitar straight into a Focusrite Scarlett with no amp, and that's still easy. It works with any interface.

Written in Rust with [cpal](https://github.com/RustAudio/cpal) (audio) and [egui](https://github.com/emilk/egui) (UI).

## Features

**Recording**
- **Multitrack layering**: every recording becomes a new track, and you overdub while the others play
- **Live waveform** while you record
- **Count-in** (4 clicks) and metronome with a beat grid
- **⟲ Capture**: forgot to hit record? Press **C** to turn the last minute you played into a take, lined up with the song if it was playing
- **🎧 Monitor**: hear yourself live through the app and its **Input effects** (turn it off if you use your interface's Direct Monitor button)
- Takes are always **recorded clean** (DI), so you can change the sound later
- Latency compensation so new takes line up with old ones

**Editing** (Reaper-style shortcuts)
- Clips on tracks: **click** to select, **drag** to move (also onto another track), **drag an edge** to trim
- **S** splits at the playhead, **Delete** deletes, **Ctrl+D** duplicates, **←/→** nudges
- **Undo/redo everything** (Ctrl+Z / Ctrl+Shift+Z), including deleting tracks
- **Right-click** menus on clips and tracks, **Snap** to beats (hold Alt to ignore)
- Per-track volume, pan, mute, solo, polarity flip, and colours

**Effects**
- Effect chains on **every track**, on the **Input** (what you hear and play through live; new takes start with a copy) and on the **Master** bus
- Built in: **EQ**, **Compressor**, **Noise gate**, **Limiter**, **Drive**, **Chorus**, **Delay** (with ping-pong), **Reverb**, **Utility** (gain and width), and **Acoustic sim** (makes an electric guitar DI sound like an acoustic)
- Turn effects on and off, reorder them, and double-click any knob to reset it. Every change can be undone.
- **Save and load chains as presets**
- Click a track's **FX** button (or any clip) to see its effects

**Safety**
- **Background autosave** every 30 s (only new audio gets written, so it never freezes). If Unplugged crashes, it offers to recover your session next time.
- Asks before closing with unsaved work

**Sharing**
- **Share / Export** presets for YouTube, Shorts, TikTok, Reels, X/Facebook, SoundCloud, MP3, Discord and Master WAV (see below)

**Setup**
- Automatically picks a Scarlett and its instrument input (Input 2 on a Scarlett Solo), and remembers your devices
- Handles interfaces whose input and output run at different sample rates

## Things other DAWs never fixed

We looked for simple features users have begged for on DAW forums for years. Several are in Unplugged now:

| Request | Asked of | Since | In Unplugged |
|---|---|---|---|
| Undo "delete track" | Pro Tools (1,000+ votes) | 2010 | ✅ everything is undoable |
| Autosave that doesn't freeze the app | Studio One, Ableton Live | 2009–2016 | ✅ background autosave + crash recovery |
| Keep what you played before hitting record (audio) | Bitwig, most DAWs | years | ✅ ⟲ Capture |
| Polarity flip on every track | Pro Tools | 2009 | ✅ track menu |
| Save and load effect chains | Pro Tools | 2009 | ✅ chain presets |
| Loudness (LUFS) for streaming | Ableton Live | ~2015 | ½ Share matches −14 LUFS; a live meter is planned |

Sources and the full list: [ROADMAP.md](ROADMAP.md#things-other-daws-never-fixed).

## Share / Export

**File → Share** (or the **Share** button). Pick where it's going, optionally a title, cover picture and a clip range, then hit **Export**.

- **Loudness matching**: measures EBU R128 loudness and matches it to −14 LUFS (what YouTube, Spotify and SoundCloud play at), with a true-peak limiter at −1 dBTP so nothing distorts after upload.
- **Audio**: Master WAV (24-bit), FLAC (CD quality, for SoundCloud/Bandcamp), MP3 320k, or small MP3 128k for Discord/messages. These are built in.
- **Video** (YouTube 16:9, Shorts/TikTok/Reels 9:16, X/Facebook 1:1): your cover picture (or a plain background), a live waveform and your title, as H.264 + AAC 384 kbps MP4. Needs [FFmpeg](https://ffmpeg.org), which is free: `winget install Gyan.FFmpeg`, or drop `ffmpeg.exe` next to `unplugged.exe`.

### Use it with any DAW: `unplugged-share`

The exporter is also a command-line tool, so you can use it on a WAV from Audacity, Reaper or anything else:

```sh
cargo build --release          # builds target/release/unplugged-share(.exe)
unplugged-share --list
unplugged-share song.wav --preset tiktok --title "Wonderwall (acoustic)" --cover art.png --from 42 --to 72
unplugged-share song.wav -p soundcloud
```

## Getting started

1. Install Rust: <https://rustup.rs>
2. Build and run:
   ```sh
   cargo run --release
   ```
3. Plug your guitar into the **instrument** input. On a **Scarlett Solo** that's the jack input (Input 2). Press the **INST** button if your model has one.
4. Use either the Scarlett's **Direct Monitor** button or Unplugged's **🎧 Monitor**, not both, or you'll hear yourself twice.
5. Play and check the **In** meter moves. Set the gain knob on the Scarlett so the halo stays green.

### Shortcuts (press F1 in the app)

| Key | Action |
|---|---|
| Space | Play / pause |
| R | Record a new layer / stop |
| C | Capture the last minute you played |
| Home / End | Jump to start / end |
| S | Split at the playhead |
| Delete / Backspace | Delete selected clip or track |
| Ctrl+D | Duplicate |
| Ctrl+Z / Ctrl+Shift+Z | Undo / redo |
| Left / Right arrow | Nudge selected clip 10 ms (Shift: 1 ms) |
| + / − or Ctrl+wheel | Zoom |
| Ctrl+S | Save project |
| Right-click | Clip / track menu |

## Latency

On Windows the default driver is WASAPI, which typically adds 20–30 ms round trip. That's playable, but you may feel it. For much lower latency, build with **ASIO** (Focusrite ships an ASIO driver):

1. Download the [ASIO SDK](https://www.steinberg.net/developers/) and set `CPAL_ASIO_DIR` to its folder
2. Install LLVM/Clang (needed by the bindings generator)
3. `cargo run --release --features asio`, then pick **ASIO** under *Audio settings > Driver*

If a new take sounds late against earlier ones, raise **Latency ms** (or lower it if early). You can also nudge clips with the arrow keys.

## Project layout

```
src/
  main.rs     window setup
  app.rs      UI (transport, side panel, timeline, editing, windows)
  model.rs    the song: tracks, clips, edit operations, undo/redo
  engine.rs   real-time engine: monitoring, playback, recording, count-in, capture, mixdown
  fx.rs       built-in effects and effect chains
  dsp.rs      filters, FFT convolution and the acoustic-sim DSP
  audio.rs    cpal device/stream handling, Scarlett auto-detection, resampling
  project.rs  project save/load (incl. autosave), WAV import/export
  share.rs    social-media export: loudness, encoders, video
  bin/unplugged-share.rs   command-line exporter
tests/ui.rs   offscreen UI tests (click, drag, delete, undo, record) with screenshots
```

Run tests with `cargo test`. UI test screenshots land in `target/ui-shots/`.

## Contributing

Issues and PRs welcome. See [ROADMAP.md](ROADMAP.md) for the plan.

## License

MIT, see [LICENSE](LICENSE).
