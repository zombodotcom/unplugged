# Unplugged Roadmap

Where this is going: **record at home → sounds good → share anywhere in one click**, all free and open source.

Legend: size **S** is about a day, **M** a few days, **L** a week or more. ✅ = done.

---

## Phase 0: The recorder (v0.1) ✅

- ✅ Live acoustic sim (pickup EQ, body IR convolution, room), loadable IRs
- ✅ Multitrack record and overdub, volume/pan/mute/solo, per-track acoustic toggle
- ✅ Metronome, latency compensation, nudge
- ✅ Save/open project, import WAV, export 24-bit WAV mix
- ✅ Scarlett auto-detect, mismatched sample-rate handling, remembers devices

## Phase 1: Make recording nicer

| Feature | Why | Size |
|---|---|---|
| **Tuner** | Every guitar session starts with one | S |
| **Count-in** (1 bar before recording) | Easier to start takes on the beat | S |
| **Auto latency calibration** (loopback ping out → in) | Takes line up without guessing the "Latency ms" slider | M |
| **Undo/redo** | Deleting a take by accident hurts | M |
| **Trim/split/move clips** and fades | Cut out bad starts and endings | M |
| **Loop recording** (record a region repeatedly, keep the best take) | The main layering workflow | M |
| **Autosave takes to disk while recording** | Never lose a take to a crash | S |
| **Prebuilt Windows releases with ASIO** (GitHub Actions) | Low latency without installing Rust or the ASIO SDK | M |
| More acoustic voices (dreadnought, parlor, nylon) as built-in IRs | Different acoustic flavours | S |

## Phase 2: Share exporter (the big one)

Goal: one **Share** button that produces a correctly formatted file for each platform, plus a **standalone open-source CLI/library** that any DAW's users can run (Audacity, Reaper, anything that exports a WAV).

### 2a. Export presets and encoders

Each preset sets container, codec, sample rate, bitrate, loudness, peak ceiling, max length and video shape.

| Preset | Output | Notes |
|---|---|---|
| Master | WAV 24-bit / FLAC | Archive, Bandcamp, distributors |
| YouTube | MP4 (H.264 + AAC-LC 384 kbps, 48 kHz), 16:9 | YouTube recommends AAC-LC or Opus at 48 kHz, 384 kbps stereo ([YouTube](https://support.google.com/youtube/answer/1722171?hl=en)) |
| YouTube Shorts / TikTok / Reels | MP4 9:16, H.264 + AAC | Reels need 9:16, H.264/HEVC ([Phyllo](https://www.getphyllo.com/post/a-complete-guide-to-the-instagram-reels-api)) |
| SoundCloud | FLAC or WAV | Lossless upload, platform transcodes |
| Discord / text | MP3 or Opus, sized to fit upload limits | Quick "listen to this riff" shares |
| X / Facebook | MP4 16:9 or 1:1 | |

- **Loudness**: measure with EBU R128 ([`ebur128`](https://crates.io/crates/ebur128) crate). Normalise to about **−14 LUFS** with a true-peak limiter at **−1 dBTP**. YouTube, Spotify and SoundCloud play back at about −14 and Apple Music at −16 ([forasoft](https://www.forasoft.com/learn/audio-for-video/articles-audio/lufs-targets-per-platform-2026)). YouTube advises staying under −1 dBTP to avoid AAC clipping ([peak-studios](https://www.peak-studios.de/en/youtube-audio-richtlinien-streaming-2025/)). TikTok and Instagram publish no official target, so −14 is a safe default. **S–M**
- **Encoders**: FLAC via [`flacenc`](https://github.com/yotarok/flacenc-rs) (pure Rust), MP3 via [`mp3lame-encoder`](https://crates.io/crates/mp3lame-encoder) (LAME), plus Opus. AAC and H.264 go through **FFmpeg as a separate program**, which keeps Unplugged MIT-licensed without linking GPL/patent-encumbered code. **M**
- **Clip picker**: drag a region on the timeline to export a 15–60 s social clip, with a fade in/out. **S**

### 2b. Video for audio-only songs

Social platforms want video. Generate one automatically:
- Waveform or spectrum visualizer, title, artist, cover image or solid colour, and a progress bar
- 16:9 and 9:16 layouts, rendered frame by frame and piped into FFmpeg. **M–L**

### 2c. OBS / camera ("show me playing")

1. **OBS sync (recommended first)**: OBS 28+ has a built-in WebSocket server that apps can use to start and stop recording ([OBS WebSocket guide](https://www.videosdk.live/developer-hub/websocket/obs-websocket)). When you hit Rec in Unplugged, OBS starts recording the camera too. Afterwards Unplugged **swaps the camera's audio for the clean studio mix**, lined up by timestamp, giving you a video with perfect sound. **M**
2. **Live streaming**: OBS can capture Unplugged directly with *Application Audio Capture* (OBS 28+, Windows 10 2004+/11) ([OBS KB](https://obsproject.com/kb/application-audio-capture-guide)). This just needs a docs page. **S**
3. **Built-in webcam recording** (later, optional), for people without OBS. **L**

### 2d. Direct upload (opt-in, per platform)

APIs differ a lot. Be realistic:

| Platform | API reality | Plan |
|---|---|---|
| YouTube | Data API v3. OAuth; default quota allows about 100 uploads/day per project ([Phyllo](https://www.getphyllo.com/post/youtube-api-limits-how-to-calculate-api-usage-cost-and-fix-exceeded-api-quota)) | **Direct upload**, the best first target |
| TikTok | Content Posting API. Unaudited apps can only post **private** videos until they pass TikTok's audit ([TikTok docs](https://developers.tiktok.com/doc/content-posting-api-get-started-upload-content/)) | Upload to the user's TikTok inbox/drafts; apply for audit later |
| Instagram / Facebook | Graph API needs a Business account and a publicly hosted video URL, 25 posts/day ([Phyllo](https://www.getphyllo.com/post/a-complete-guide-to-the-instagram-reels-api)) | Export the file plus "open upload page"; no direct upload at first |
| X | Paid API | Export and open the page |
| SoundCloud / Bandcamp | Limited or no public upload API | Export and open the page |

OAuth tokens are stored in the OS keychain and never in project files.

### 2e. Standalone exporter crate and CLI

Split 2a–2b into its own crate, `unplugged-share`, with a CLI:

```sh
unplugged-share song.wav --preset tiktok --cover art.png --title "Wonderwall (acoustic)" --clip 0:42-1:12
```

This makes it an **open-source social exporter for any DAW**, not just Unplugged. Other projects can use it as a library. **M**

---

## Extra plan: a shareable "re-toneable" song format

**Idea:** a song file that plays like a normal audio file anywhere, but also carries the **clean DI guitar layers and tone settings**. Anyone opening it in Unplugged (or a web player) can solo layers, switch acoustic/electric, retone, remix or learn the parts.

### Lessons from prior art
- **NI Stems** put a normal stereo mix plus 4 stems in one `.stem.mp4`, which plays in any MP4 player ([Native Instruments](https://www.native-instruments.com/en/specials/stems/)). Adoption stalled though ([Remix.me](https://remix.me/en/blog/native-instruments-stems/)); the 4-stem cap and DJ-only focus hurt.
- **DAWproject** is an open project-exchange format from Bitwig and PreSonus. Bitwig, Studio One and Cubase 14 support it, and Reaper has a tool ([Bitwig FAQ](https://www.bitwig.com/support/technical_support/dawproject-file-format-faqs-62/)).

### Design: `.unplugged.m4a` (working name)
1. **Track 1 = the finished stereo mix (AAC)**, marked as the default. It plays in any phone, browser or player. Compatibility comes first; this is the lesson from Stems.
2. **Tracks 2…n = each clean DI layer** (FLAC or Opus), with no 4-stem limit.
3. **Embedded JSON metadata**: BPM, per-track pan/volume, acoustic-sim settings, IR name and hash, and optional chords/tab/lyrics.
4. Tools:
   - Unplugged opens it as a full project, so "send me your song" becomes "here's the session"
   - **Web player** (Unplugged's DSP compiled to WASM): toggle layers and acoustic/electric in the browser, shareable by link
   - Converters: **export to NI Stems** (fold into 4) and **export to DAWproject** (open in Bitwig, Studio One or Cubase)
5. Publish the spec in `docs/format.md` under a permissive licence so other apps can adopt it.

| Step | Size |
|---|---|
| Spec draft and a reference writer/reader crate | M |
| Open in Unplugged | S |
| DAWproject export | M |
| NI Stems export | S |
| WASM web player | L |

---

## Suggested order

1. Phase 1 quick wins: tuner, count-in, autosave, calibration
2. **2a presets + loudness + FLAC/MP3**: the first real "share" button
3. **2c OBS sync**: a video of you playing, with studio sound
4. 2b visualizer videos → 2e standalone CLI
5. 2d YouTube upload, then TikTok drafts
6. Extra plan: song format → DAWproject export → web player

Want to help? Pick a row and open an issue.
