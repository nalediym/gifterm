//! # gifterm
//!
//! Play GIF animations in kitty-protocol terminals.
//!
//! This crate provides the core logic for decoding GIF files, caching decoded
//! frames, and transmitting them via the
//! [kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).
//!
//! ## Library usage
//!
//! ```rust,no_run
//! use std::path::Path;
//!
//! let path = Path::new("animation.gif");
//! let (meta, frames) = gifterm::load_frames(path, Some(400)).unwrap();
//! gifterm::play(&meta, &frames).unwrap();
//! ```
//!
//! ## Feature flags
//!
//! - **`cli`** -- Enables the `clap` dependency for the binary CLI. Disabled by
//!   default so the library can compile to `wasm32`.

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use image::{AnimationDecoder, codecs::gif::GifDecoder, imageops::FilterType};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during GIF decoding or playback.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An I/O error occurred.
    Io(io::Error),
    /// The image crate returned an error during decoding.
    Image(image::ImageError),
    /// JSON (de)serialisation failed (cache metadata).
    Json(serde_json::Error),
    /// The GIF has fewer than 2 frames.
    TooFewFrames,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Image(e) => write!(f, "image error: {e}"),
            Error::Json(e) => write!(f, "JSON error: {e}"),
            Error::TooFewFrames => write!(f, "need at least 2 frames for animation"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Image(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::TooFewFrames => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<image::ImageError> for Error {
    fn from(e: image::ImageError) -> Self {
        Error::Image(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

// ---------------------------------------------------------------------------
// Cache metadata
// ---------------------------------------------------------------------------

/// Metadata about a decoded and cached GIF animation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    /// Width of each frame in pixels.
    pub width: u32,
    /// Height of each frame in pixels.
    pub height: u32,
    /// Total number of frames.
    pub n_frames: usize,
    /// Per-frame delay in milliseconds.
    pub durations: Vec<u32>,
    /// Original source filename (for display purposes).
    pub source: String,
}

// ---------------------------------------------------------------------------
// Kitty graphics protocol helpers
// ---------------------------------------------------------------------------

/// Build a kitty graphics protocol escape sequence.
pub fn gr_cmd(params: &str, payload: Option<&str>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"\x1b_G");
    buf.extend_from_slice(params.as_bytes());
    if let Some(data) = payload {
        buf.push(b';');
        buf.extend_from_slice(data.as_bytes());
    }
    buf.extend_from_slice(b"\x1b\\");
    buf
}

/// Transmit RGBA data to the terminal via a temp file (`t=t` transfer).
pub fn send_via_file(out: &mut impl Write, params: &str, rgba_data: &[u8]) -> io::Result<()> {
    let mut tmp = NamedTempFile::with_prefix_in("gifterm_", "/tmp")?;
    tmp.write_all(rgba_data)?;
    let path = tmp.into_temp_path();

    let path_b64 = B64.encode(path.to_str().unwrap().as_bytes());

    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"\x1b_G");
    buf.extend_from_slice(params.as_bytes());
    buf.extend_from_slice(b",t=t;");
    buf.extend_from_slice(path_b64.as_bytes());
    buf.extend_from_slice(b"\x1b\\");

    out.write_all(&buf)?;
    out.flush()?;

    // Keep the file so kitty can read it (kitty deletes temp files with t=t)
    path.keep().ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Unique image IDs
// ---------------------------------------------------------------------------

/// Generate a unique image ID from current time (kitty supports u32 IDs).
pub fn unique_image_id() -> u32 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    // Keep in range 1..=u32::MAX, avoid 0
    (t % 0xFFFF_FFFE) + 1
}

// ---------------------------------------------------------------------------
// Cache layer
// ---------------------------------------------------------------------------

/// Return the cache directory path (`$XDG_CACHE_HOME/gifterm` or
/// `~/.cache/gifterm`).
pub fn cache_dir() -> PathBuf {
    let base = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else {
        let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
        p.push(".cache");
        p
    };
    base.join("gifterm")
}

/// Compute a SHA-256 based hash of a file (first 16 hex chars).
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash = format!("{:x}", hasher.finalize());
    Ok(hash[..16].to_string())
}

/// Build a cache key from the file hash and optional max width.
pub fn cache_key(path: &Path, max_width: Option<u32>) -> io::Result<String> {
    let mut key = hash_file(path)?;
    if let Some(w) = max_width {
        key.push_str(&format!("_w{w}"));
    }
    Ok(key)
}

/// Try to load previously cached frames from disk.
pub fn load_from_cache(cache_path: &Path) -> Option<(Meta, Vec<Vec<u8>>)> {
    let meta_str = fs::read_to_string(cache_path.join("meta.json")).ok()?;
    let meta: Meta = serde_json::from_str(&meta_str).ok()?;

    let mut frames = Vec::with_capacity(meta.n_frames);
    for i in 0..meta.n_frames {
        let data = fs::read(cache_path.join(format!("{i:04}.rgba"))).ok()?;
        frames.push(data);
    }
    Some((meta, frames))
}

/// Decode a GIF file, optionally scale it, and write the result to the cache.
pub fn decode_and_cache(
    gif_path: &Path,
    max_width: Option<u32>,
    cache_path: &Path,
) -> Result<(Meta, Vec<Vec<u8>>), Error> {
    eprintln!("Decoding {}...", gif_path.display());

    let file = BufReader::new(fs::File::open(gif_path)?);
    let decoder = GifDecoder::new(file)?;
    let raw_frames: Vec<_> = decoder.into_frames().collect::<Result<Vec<_>, _>>()?;

    if raw_frames.len() < 2 {
        return Err(Error::TooFewFrames);
    }

    let first = raw_frames[0].buffer();
    let (orig_w, orig_h) = (first.width(), first.height());

    let (out_w, out_h) = if let Some(mw) = max_width {
        if orig_w > mw {
            let scale = mw as f64 / orig_w as f64;
            (mw, (orig_h as f64 * scale) as u32)
        } else {
            (orig_w, orig_h)
        }
    } else {
        (orig_w, orig_h)
    };

    let mut frames = Vec::with_capacity(raw_frames.len());
    let mut durations = Vec::with_capacity(raw_frames.len());

    for (i, frame) in raw_frames.iter().enumerate() {
        let (numer, denom) = frame.delay().numer_denom_ms();
        let ms = (numer as u32 / denom as u32).max(20);
        durations.push(ms);

        let img = frame.buffer().clone();
        let rgba = if out_w != orig_w || out_h != orig_h {
            image::imageops::resize(&img, out_w, out_h, FilterType::Lanczos3)
        } else {
            img
        };
        frames.push(rgba.into_raw());

        if (i + 1) % 20 == 0 || i == raw_frames.len() - 1 {
            eprint!("\r  Decoded {}/{}", i + 1, raw_frames.len());
        }
    }
    eprintln!();

    // Write cache
    fs::create_dir_all(cache_path)?;
    let meta = Meta {
        width: out_w,
        height: out_h,
        n_frames: frames.len(),
        durations,
        source: gif_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    fs::write(
        cache_path.join("meta.json"),
        serde_json::to_string_pretty(&meta)?,
    )?;
    for (i, frame_data) in frames.iter().enumerate() {
        fs::write(cache_path.join(format!("{i:04}.rgba")), frame_data)?;
    }

    let cache_kb: usize = frames.iter().map(|f: &Vec<u8>| f.len()).sum::<usize>() / 1024;
    eprintln!(
        "  Cached {} frames ({} KB) -> {}",
        frames.len(),
        cache_kb,
        cache_path.display()
    );

    Ok((meta, frames))
}

/// Load frames for a GIF, using the cache if available.
pub fn load_frames(gif_path: &Path, max_width: Option<u32>) -> Result<(Meta, Vec<Vec<u8>>), Error> {
    let key = cache_key(gif_path, max_width)?;
    let cp = cache_dir().join(&key);

    if let Some(result) = load_from_cache(&cp) {
        eprintln!(
            "Cache hit ({}): {} frames, {}x{}",
            key, result.0.n_frames, result.0.width, result.0.height
        );
        return Ok(result);
    }

    decode_and_cache(gif_path, max_width, &cp)
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

/// Play a decoded GIF animation via the kitty graphics protocol.
///
/// Frames are transmitted to the terminal as temp files, then assembled into
/// a looping animation that kitty manages on the GPU side.
pub fn play(meta: &Meta, frames: &[Vec<u8>]) -> io::Result<()> {
    let out = io::stdout();
    let mut out = out.lock();
    let id = unique_image_id();
    let w = meta.width;
    let h = meta.height;

    eprintln!("  {} frames, {}x{}", meta.n_frames, w, h);
    eprint!("  Sending frames...");

    // Frame 1: transmit + display
    send_via_file(
        &mut out,
        &format!("a=T,i={id},f=32,s={w},v={h},q=2"),
        &frames[0],
    )?;

    // Frames 2+
    for (i, (frame_data, dur)) in frames[1..].iter().zip(&meta.durations[1..]).enumerate() {
        send_via_file(
            &mut out,
            &format!("a=f,i={id},f=32,s={w},v={h},z={dur},q=2"),
            frame_data,
        )?;
        if (i + 1) % 10 == 0 || i == frames.len() - 2 {
            eprint!("\r  Sending frames... {}/{}", i + 2, frames.len());
        }
    }
    eprintln!();

    // Set gap for root frame
    let d0 = meta.durations[0];
    out.write_all(&gr_cmd(&format!("a=a,i={id},r=1,z={d0},q=2"), None))?;

    // Start infinite loop
    out.write_all(&gr_cmd(&format!("a=a,i={id},s=3,v=1,q=2"), None))?;
    out.flush()?;

    eprintln!("  Playing! Animation lives in kitty until you clear the screen.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Terminal detection (platform-specific, not available on wasm)
// ---------------------------------------------------------------------------

/// Check if the terminal supports the kitty graphics protocol by probing
/// environment variables and, if needed, sending a graphics protocol query.
#[cfg(not(target_arch = "wasm32"))]
pub fn check_kitty_support() -> bool {
    use std::time::Duration;

    // Quick check: TERM or TERM_PROGRAM often reveals kitty/wezterm
    if let Ok(term) = std::env::var("TERM") {
        if term.contains("kitty") {
            return true;
        }
    }
    if let Ok(prog) = std::env::var("TERM_PROGRAM") {
        let p = prog.to_lowercase();
        if p.contains("kitty") || p.contains("wezterm") {
            return true;
        }
    }

    // Send a graphics protocol query: 1x1 red pixel, action=query
    let query = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

    let tty = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(f) => f,
        Err(_) => return false,
    };

    let fd = {
        use std::os::unix::io::AsRawFd;
        tty.as_raw_fd()
    };

    let old_termios = unsafe {
        let mut t = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return false;
        }
        t
    };

    let mut raw = old_termios;
    unsafe {
        libc::cfmakeraw(&mut raw);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1; // 100ms timeout
        libc::tcsetattr(fd, libc::TCSANOW, &raw);
    }

    // Write query
    {
        use std::os::unix::io::FromRawFd;
        let mut writer = unsafe { fs::File::from_raw_fd(fd) };
        let _ = writer.write_all(query);
        let _ = writer.flush();
        std::mem::forget(writer);
    }

    std::thread::sleep(Duration::from_millis(150));

    let mut response = vec![0u8; 256];
    let n = unsafe { libc::read(fd, response.as_mut_ptr() as *mut libc::c_void, 256) };

    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &old_termios);
    }

    if n > 0 {
        let resp = String::from_utf8_lossy(&response[..n as usize]);
        resp.contains("OK")
    } else {
        false
    }
}

/// Find the kitty binary on disk.
#[cfg(not(target_arch = "wasm32"))]
pub fn find_kitty() -> Option<PathBuf> {
    use std::process::Command;

    let candidates = [
        "/opt/homebrew/bin/kitty",
        "/usr/local/bin/kitty",
        "/usr/bin/kitty",
        "/Applications/kitty.app/Contents/MacOS/kitty",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Some(PathBuf::from(c));
        }
    }
    Command::new("which")
        .arg("kitty")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_id_is_nonzero() {
        for _ in 0..100 {
            assert_ne!(unique_image_id(), 0);
        }
    }

    #[test]
    fn gr_cmd_without_payload() {
        let cmd = gr_cmd("a=T,i=1", None);
        assert_eq!(cmd, b"\x1b_Ga=T,i=1\x1b\\");
    }

    #[test]
    fn gr_cmd_with_payload() {
        let cmd = gr_cmd("a=T,i=1", Some("AAAA"));
        assert_eq!(cmd, b"\x1b_Ga=T,i=1;AAAA\x1b\\");
    }

    #[test]
    fn cache_key_without_width() {
        // Just verify the function signature works -- actual hashing needs a real file
        let result = cache_key(std::path::Path::new("/nonexistent"), None);
        assert!(result.is_err());
    }
}
