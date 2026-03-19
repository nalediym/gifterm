use clap::Parser;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use gifterm::{check_kitty_support, find_kitty};

// -- CLI --

#[derive(Parser)]
#[command(
    name = "gifterm",
    about = "Play GIF animations in kitty-protocol terminals"
)]
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

// -- Terminal helpers (CLI-only) --

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
                    eprintln!("Failed to launch kitty: {e}");
                    std::process::exit(1);
                }
            }
        }
        std::process::exit(1);
    }

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
                _ => eprintln!(
                    "Installation failed. Install manually: https://sw.kovidgoyal.net/kitty/"
                ),
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

    let (meta, frames) = match gifterm::load_frames(&cli.gif, cli.width) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    if cli.cache_only {
        eprintln!("Cached. Not playing (--cache-only).");
        return;
    }

    if let Err(e) = gifterm::play(&meta, &frames) {
        eprintln!("Playback error: {e}");
        std::process::exit(1);
    }
}
