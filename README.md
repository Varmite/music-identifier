# Music Identifier

A small, fast desktop app that recognises the song playing in **one specific window**
(a browser tab, Spotify, a game, a video player…) without picking up anything else.

- **Rust** backend + **egui** GPU-rendered GUI: one ~7 MB native executable, no runtime, no webview
- Captures audio **per application**, not the whole system mix
- Native Shazam-compatible fingerprinting (no API key needed)
- "Keep listening" mode follows playlists and logs every new song
- History with cover art, plus links to Shazam / Apple Music / Spotify / YouTube

## Download

Grab the build for your OS from the [Releases](../../releases) page:
Windows (x64), macOS (Apple Silicon / Intel `.app`), Linux (x64).

## How it captures one window's audio

| OS | Mechanism | Requirement |
|---|---|---|
| Windows | WASAPI *process loopback* on the window's process tree | Windows 10 2004+ / Windows 11 |
| macOS | ScreenCaptureKit audio capture filtered to the window's app | macOS 13+, Screen Recording permission |
| Linux | PulseAudio monitor of the app's playback stream (works on PipeWire via `pipewire-pulse`) | `pactl` + `parec` (`pulseaudio-utils`) |

On Windows the source list marks windows that are currently producing sound with a green **•** and sorts them first.
On Linux, apps are listed by their audio streams (browsers usually name these after the tab), because
Wayland doesn't let apps list other windows.

## Build

```sh
cargo build --release
# → target/release/music-identifier(.exe)
```

Linux needs a C compiler (for rustls' `ring`) and, at runtime, the usual GUI libraries
(`libxkbcommon`, `libGL`/Mesa, `libwayland` or X11) plus `pulseaudio-utils`.

On macOS the Screen Recording permission is granted to whichever app launches the binary
(e.g. Terminal) unless you package it as an `.app` (e.g. with `cargo bundle`).

## Usage

1. Choose a window/stream from the **Source** dropdown (use the filter box to search).
2. Press the big button. Most songs are recognised within 4–10 seconds.
3. With **Keep listening** on, it keeps checking every ~15 s and adds each new song to the history.

History is stored in your data directory (`%APPDATA%\music-identifier`,
`~/Library/Application Support/music-identifier`, or `~/.local/share/music-identifier`).

### CLI helpers

```sh
music-identifier --list                    # list capturable sources
music-identifier --listen "Spotify"        # identify what a source (title/app substring or PID) is playing
music-identifier --identify-raw clip.raw   # identify raw 16 kHz mono s16le PCM
```

## Code layout

```
src/
  main.rs         entry point + CLI helpers
  app.rs          egui UI
  engine.rs       capture → 16 kHz ring buffer → periodic recognition
  signature.rs    Shazam-compatible fingerprint (port of SongRec's algorithm)
  shazam.rs       recognition request + response parsing
  resample.rs     down-mix + windowed-sinc resampler
  capture/        windows.rs · macos.rs · linux.rs
```

## Notes

Recognition uses Shazam's unofficial endpoint, the same one used by
[SongRec](https://github.com/marin-m/SongRec), whose fingerprinting algorithm this port is
based on (hence the GPL-3.0 licence). It is not an official API and could change without notice.
