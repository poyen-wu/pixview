mod archive;
mod browser;
mod ffmpeg;
mod sixel;
mod util;
mod viewer;

use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use crossterm::{
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};

use archive::{ArchiveType, EntryPath};
use browser::{resolve_single_dir, show_browser};
use util::is_video;
use viewer::{show_image, show_video};

#[derive(Parser)]
#[command(name = "pixview", about = "Display images and video thumbnails using sixel")]
struct Cli {
    /// Image/video file or directory to display
    #[arg(default_value = ".")]
    path: String,

    /// Maximum color registers for sixel quantization (4-65534)
    #[arg(short, long, default_value = "256")]
    colors: usize,
}

struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn main() -> Result<()> {
    ffmpeg::init()?;

    let cli = Cli::parse();

    let path = Path::new(&cli.path);
    let abs_path = path.canonicalize().unwrap_or_else(|_| PathBuf::from(&cli.path));

    let start_arc_ty = abs_path
        .file_name()
        .and_then(|n| archive::archive_type(&n.to_string_lossy()));

    let mut start_path = if abs_path.is_file() {
        match start_arc_ty {
            Some(ArchiveType::Zip) => EntryPath::InZip(abs_path.clone(), String::new()),
            Some(ArchiveType::Rar) => EntryPath::InRar(abs_path.clone(), String::new()),
            None => EntryPath::Native(abs_path),
        }
    } else {
        EntryPath::Native(abs_path)
    };

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = TermGuard;

    if let EntryPath::Native(p) = &mut start_path {
        if p.is_file() && start_arc_ty.is_none() {
            let file_name = p.file_name().unwrap_or_default().to_string_lossy();
            if is_video(&file_name) {
                let _ = show_video(&mut stdout, &EntryPath::Native(p.clone()), cli.colors);
            } else {
                let _ = show_image(&mut stdout, &EntryPath::Native(p.clone()), cli.colors);
            }
            return Ok(());
        }
    }

    start_path = resolve_single_dir(start_path);
    show_browser(&mut stdout, start_path, cli.colors)?;

    Ok(())
}
