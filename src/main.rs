use base64::{Engine, engine::general_purpose::STANDARD as B64};
use clap::Parser;
use image::{AnimationDecoder, codecs::gif::GifDecoder, imageops::FilterType};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

/// Generate a unique image ID from current time (kitty supports u32 IDs).
fn unique_image_id() -> u32 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    // Keep in range 1..=u32::MAX, avoid 0
    (t % 0xFFFF_FFFE) + 1
}

// -- CLI --

#[derive(Parser)]
#[command(name = "gifterm", about = "Play GIF animations in kitty-protocol terminals")]
struct Cli {
    /// GIF file to play
    gif: PathBuf,

    /// Max pixel width (scales down)
    #[arg(long)]
    width: Option<u32>,

    /// Only decode and cache, don't play
    #[arg(long)]
    cache_only: bool,
}

// -- Cache metadata --

#[derive(serde::Serialize, serde::Deserialize)]
struct Meta {
    width: u32,
    height: u32,
    n_frames: usize,
    durations: Vec<u32>,
    source: String,
}

// -- Kitty graphics protocol --

/// Build a kitty graphics protocol escape sequence.
/// Params are key=value pairs written directly into the sequence.
fn gr_cmd(params: &str, payload: Option<&str>) -> Vec<u8> {
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

fn send_via_file(
    out: &mut impl Write,
    params: &str,
    rgba_data: &[u8],
) -> io::Result<()> {
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

// -- Cache --

fn cache_dir() -> PathBuf {
    let base = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else {
        let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
        p.push(".cache");
        p
    };
    base.join("gifterm")
}

fn hash_file(path: &Path) -> io::Result<String> {
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

fn cache_key(path: &Path, max_width: Option<u32>) -> io::Result<String> {
    let mut key = hash_file(path)?;
    if let Some(w) = max_width {
        key.push_str(&format!("_w{}", w));
    }
    Ok(key)
}

fn load_from_cache(cache_path: &Path) -> Option<(Meta, Vec<Vec<u8>>)> {
    let meta_str = fs::read_to_string(cache_path.join("meta.json")).ok()?;
    let meta: Meta = serde_json::from_str(&meta_str).ok()?;

    let mut frames = Vec::with_capacity(meta.n_frames);
    for i in 0..meta.n_frames {
        let data = fs::read(cache_path.join(format!("{:04}.rgba", i))).ok()?;
        frames.push(data);
    }
    Some((meta, frames))
}

fn decode_and_cache(
    gif_path: &Path,
    max_width: Option<u32>,
    cache_path: &Path,
) -> Result<(Meta, Vec<Vec<u8>>), Box<dyn std::error::Error>> {
    eprintln!("Decoding {}...", gif_path.display());

    let file = BufReader::new(fs::File::open(gif_path)?);
    let decoder = GifDecoder::new(file)?;
    let raw_frames: Vec<_> = decoder.into_frames().collect::<Result<Vec<_>, _>>()?;

    if raw_frames.len() < 2 {
        return Err("Need at least 2 frames for animation".into());
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
        fs::write(cache_path.join(format!("{:04}.rgba", i)), frame_data)?;
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

fn load_frames(
    gif_path: &Path,
    max_width: Option<u32>,
) -> Result<(Meta, Vec<Vec<u8>>), Box<dyn std::error::Error>> {
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

// -- Playback --

fn play(meta: &Meta, frames: &[Vec<u8>]) -> io::Result<()> {
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

// -- Terminal detection --

/// Check if the terminal supports the kitty graphics protocol by sending
/// a 1x1 query image and reading the response.
fn check_kitty_support() -> bool {
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
    // A compatible terminal responds with \x1b_G...OK...\x1b\\
    let query = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

    // We need raw terminal access to read the response
    let tty = match fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
        Ok(f) => f,
        Err(_) => return false,
    };

    // Put terminal in raw mode to read the response
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
        // Don't drop — we still need the fd
        std::mem::forget(writer);
    }

    // Wait briefly then read response
    std::thread::sleep(Duration::from_millis(150));

    let mut response = vec![0u8; 256];
    let n = unsafe { libc::read(fd, response.as_mut_ptr() as *mut libc::c_void, 256) };

    // Restore terminal
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

/// Find kitty binary path
fn find_kitty() -> Option<PathBuf> {
    // Common locations
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
    // Try PATH
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

/// Prompt the user for yes/no
fn prompt_yn(msg: &str) -> bool {
    eprint!("{} [Y/n] ", msg);
    io::stderr().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    let answer = input.trim().to_lowercase();
    answer.is_empty() || answer == "y" || answer == "yes"
}

/// Offer to install kitty and re-launch inside it
fn offer_kitty_install(args: &[String]) {
    eprintln!("gifterm requires a terminal with kitty graphics protocol support.");
    eprintln!("Supported terminals: kitty, WezTerm, Konsole (partial)\n");

    // Check if kitty is already installed but we're not running in it
    if let Some(kitty_path) = find_kitty() {
        eprintln!("kitty is installed at {}", kitty_path.display());
        if prompt_yn("Launch gifterm inside kitty?") {
            let gifterm = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gifterm"));
            let status = Command::new(&kitty_path)
                .arg("--hold")
                .arg(&gifterm)
                .args(&args[1..])
                .status();
            match status {
                Ok(s) => std::process::exit(s.code().unwrap_or(0)),
                Err(e) => {
                    eprintln!("Failed to launch kitty: {}", e);
                    std::process::exit(1);
                }
            }
        }
        std::process::exit(1);
    }

    // Offer to install
    let is_mac = cfg!(target_os = "macos");
    let is_linux = cfg!(target_os = "linux");

    if is_mac {
        eprintln!("Install kitty with Homebrew?");
        if prompt_yn("  brew install --cask kitty") {
            eprintln!("\nInstalling kitty...\n");
            let status = Command::new("brew")
                .args(["install", "--cask", "kitty"])
                .status();
            match status {
                Ok(s) if s.success() => {
                    eprintln!("\nkitty installed successfully!");
                    if let Some(kitty_path) = find_kitty() {
                        if prompt_yn("Launch gifterm inside kitty now?") {
                            let gifterm = std::env::current_exe()
                                .unwrap_or_else(|_| PathBuf::from("gifterm"));
                            let _ = Command::new(&kitty_path)
                                .arg("--hold")
                                .arg(&gifterm)
                                .args(&args[1..])
                                .status();
                        }
                    }
                }
                _ => eprintln!("Installation failed. Install manually: https://sw.kovidgoyal.net/kitty/"),
            }
        }
    } else if is_linux {
        eprintln!("Install kitty with:");
        eprintln!("  curl -L https://sw.kovidgoyal.net/kitty/installer.sh | sh /dev/stdin");
        eprintln!("\nOr use your package manager (apt install kitty, dnf install kitty, etc.)");
    } else {
        eprintln!("Download kitty from: https://sw.kovidgoyal.net/kitty/");
    }

    std::process::exit(1);
}

// -- Main --

fn main() {
    let cli = Cli::parse();

    // Check terminal compatibility before doing anything
    if !cli.cache_only && !check_kitty_support() {
        let args: Vec<String> = std::env::args().collect();
        offer_kitty_install(&args);
    }

    if !cli.gif.exists() {
        eprintln!("Not found: {}", cli.gif.display());
        std::process::exit(1);
    }

    let (meta, frames) = match load_frames(&cli.gif, cli.width) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if cli.cache_only {
        eprintln!("Cached. Not playing (--cache-only).");
        return;
    }

    if let Err(e) = play(&meta, &frames) {
        eprintln!("Playback error: {}", e);
        std::process::exit(1);
    }
}
