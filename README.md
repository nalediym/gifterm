# gifterm

[![Crates.io](https://img.shields.io/crates/v/gifterm)](https://crates.io/crates/gifterm)
[![docs.rs](https://docs.rs/gifterm/badge.svg)](https://docs.rs/gifterm)
[![License: MIT](https://img.shields.io/crates/l/gifterm)](LICENSE)

Play animated GIFs natively in your terminal.

**[Homepage](https://nalediym.github.io/gifterm)** | **[crates.io](https://crates.io/crates/gifterm)** | **[docs.rs](https://docs.rs/gifterm)**

![gifterm demo](demo.gif)

## Why

Terminals are where we live. They deserve to feel alive. gifterm brings lofi
vibes, pixel art, and ambient animation to your workspace -- no browser tab, no
electron app, just frames on your GPU.

## Install

```
cargo install gifterm
```

Or build from source:

```
git clone https://github.com/nalediym/gifterm.git
cd gifterm
cargo install --path .
```

Single static binary. No runtime dependencies.

## Usage

```
gifterm lofi.gif                  # play a GIF
gifterm lofi.gif --width 400      # scale down to 400px wide
gifterm lofi.gif --cache-only     # decode and cache without playing
```

Output uses a quiet `gifterm <action> <detail>` format:

```
gifterm  decoding  lofi.gif
gifterm  scaling   640x480 -> 400x300 (lanczos3)
gifterm  cached    47 frames (1840 KB) -> ~/.cache/gifterm/a1b2c3d4
gifterm  playing   47 frames, loop=infinite, id=1001
```

The animation is fire-and-forget: it persists in kitty after `gifterm` exits,
living on the GPU like an `<img>` on a webpage. Clear the screen to dismiss it.

Multiple animations can run simultaneously -- each gets a unique image ID.

## Library usage

gifterm can be used as a Rust library (without the CLI dependency):

```toml
[dependencies]
gifterm = { version = "0.1", default-features = false }
```

```rust
use std::path::Path;

let path = Path::new("animation.gif");
let (meta, frames) = gifterm::load_frames(path, Some(400)).unwrap();
gifterm::play(&meta, &frames).unwrap();
```

The library compiles without `clap` and targets `wasm32`. See [docs.rs/gifterm](https://docs.rs/gifterm) for full API documentation.

## Requirements

A terminal that supports the [kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/):

- [kitty](https://sw.kovidgoyal.net/kitty/) -- full support
- [WezTerm](https://wezfurlong.org/wezterm/) -- full support
- [Konsole](https://konsole.kde.org/) -- partial support

tmux blocks the graphics protocol by default. Set `allow-passthrough on` in
your tmux.conf, or run gifterm in a raw terminal window.

## How it works

gifterm decodes GIF frames into raw RGBA buffers using the `image` crate,
optionally scaling them down with Lanczos3 filtering. Decoded frames are cached
to `~/.cache/gifterm/` keyed by a SHA-256 hash of the source file, so
subsequent plays are instant.

Frames are transmitted to the terminal via kitty's graphics protocol using
temp-file transfer (`t=t`), then assembled into a looping animation that kitty
manages entirely on the GPU side. The CLI process exits immediately -- the
animation keeps running.

## Contributing

Contributions welcome. To get started:

```
git clone https://github.com/nalediym/gifterm.git
cd gifterm
cargo build
```

The codebase is split into `src/lib.rs` (core library) and `src/main.rs` (CLI
wrapper), with a straightforward pipeline: GIF decode -> frame cache -> kitty
graphics protocol transmission.

Some areas that could use help:

- **Sixel support** -- for terminals that don't support kitty graphics (e.g. foot, mlterm)
- **APNG / WebP** -- extend beyond GIF to other animated formats
- **Speed control** -- `--speed 2x` or `--fps 30` flags
- **Cleanup command** -- `gifterm --clear` to remove cached frames

Open an issue before starting large changes so we can align on direction.

## License

MIT
