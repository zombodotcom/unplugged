# Unplugged

A tiny, free, open-source recorder for playing **electric guitar straight into an audio interface and hearing it as an acoustic**. No amp, no mic, no effects pedals.

Plug your guitar (e.g. a Strat) into your interface (e.g. a Focusrite Scarlett), put headphones on, and play. Hit record to lay down a take, then record more takes on top while the earlier ones play back.

Written in Rust with [cpal](https://github.com/RustAudio/cpal) (audio) and [egui](https://github.com/emilk/egui) (UI).

## Features

- **Acoustic simulator**: turns a magnetic-pickup DI into an acoustic-style sound:
  - *Pickup correction EQ* cuts the low mud, mid "honk" and pickup resonance of electric pickups
  - *Body* convolves with an acoustic body impulse response (built-in synthesised body, or **load any acoustic-sim IR `.wav`**)
  - *Sparkle / Warmth* adds the top end and low "box" resonance an acoustic has and pickups lack
  - *Room* adds a small room reverb
- **Live monitoring** through the sim (turn off "Hear myself" if you use your interface's direct monitor)
- **Multitrack layering**: every recording becomes a new track, and you overdub while the others play
- Per-track volume, pan, mute, solo, and an **A** toggle to hear a track acoustic or clean
- Takes are always **recorded clean** (DI), so you can change the acoustic sound later
- Metronome with beat grid, latency compensation, and per-track nudge
- Save/open projects (folder with `project.json` + WAVs), import WAV backing tracks, export a stereo mix (24-bit WAV)
- Automatically picks a Scarlett and its instrument input (Input 2 on a Scarlett Solo)
- Handles interfaces whose input and output run at different sample rates

## Getting started

1. Install Rust: <https://rustup.rs>
2. Build and run:
   ```sh
   cargo run --release
   ```
3. Plug the guitar into the **instrument** input. On a **Scarlett Solo** that's the jack input (Input 2). Press the **INST** button if your model has one.
4. **Turn Direct Monitor off** (the button on the Scarlett), otherwise you'll hear the dry electric under the acoustic sound.
5. Play and check the **In** meter moves. Set the gain knob on the Scarlett so the halo stays green.

### Shortcuts

| Key   | Action                       |
|-------|------------------------------|
| Space | Play / pause                 |
| R     | Record a new layer / stop    |
| Home  | Back to start                |
| Ctrl + mouse wheel on timeline | Zoom |

## Latency

On Windows the default driver is WASAPI, which typically adds 20–30 ms round trip. That's playable, but you may feel it. For much lower latency, build with **ASIO** (Focusrite ships an ASIO driver):

1. Download the [ASIO SDK](https://www.steinberg.net/developers/) and set `CPAL_ASIO_DIR` to its folder
2. Install LLVM/Clang (needed by the bindings generator)
3. `cargo run --release --features asio`, then pick **ASIO** under *Audio settings > Driver*

If a new take sounds late against earlier ones, raise **Latency ms** (or lower it if early). You can also nudge single tracks.

## Better acoustic tones with real IRs

The built-in body is synthesised. A real electric-to-acoustic IR (captured by recording an acoustic with a mic and a pickup at the same time) usually sounds more realistic. Load any mono or stereo `.wav` IR up to 1 second with **Load IR…**. Try turning *Pickup correction EQ* off if the IR already includes that correction.

## Project layout

```
src/
  main.rs     window setup
  app.rs      UI (transport, sim panel, timeline)
  engine.rs   real-time engine: monitoring, playback, recording, metronome, mixdown
  dsp.rs      acoustic sim: biquads, partitioned FFT convolution, room reverb, built-in body IR
  audio.rs    cpal device/stream handling, Scarlett auto-detection, resampling
  project.rs  project save/load, WAV import/export
examples/
  devices.rs  `cargo run --example devices` lists devices as cpal sees them
```

Run tests with `cargo test`.

## Contributing

Issues and PRs welcome. See [ROADMAP.md](ROADMAP.md) for the plan: social-media export presets, OBS sync, and a shareable re-toneable song format.

## License

MIT, see [LICENSE](LICENSE).
