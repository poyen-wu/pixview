mod ffmpeg;
mod sixel;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use image::DynamicImage;

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

enum ViewerAction {
    QuitProgram,
    ReturnToBrowser,
    NextFile,
    PreviousFile,
}

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "flv", "wmv", "m4v", "mpg", "mpeg", "ts", "mts", "vob",
    "ogv", "3gp",
];

const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "ico", "tiff", "webp", "avif", "pnm", "tga",
];

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

fn terminal_cell_size() -> (u32, u32) {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    unsafe {
        let fd = libc::open(b"/dev/tty\0".as_ptr() as *const i8, libc::O_RDONLY);
        if fd >= 0 {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0
                && ws.ws_xpixel > 0
                && ws.ws_ypixel > 0
                && ws.ws_col > 0
                && ws.ws_row > 0
            {
                libc::close(fd);
                return (
                    ws.ws_xpixel as u32 / cols as u32,
                    ws.ws_ypixel as u32 / rows as u32,
                );
            }
            libc::close(fd);
        }
    }

    (10, 20)
}

fn format_time(seconds: f64) -> String {
    let total = seconds as u64;
    let hrs = total / 3600;
    let mins = (total % 3600) / 60;
    let secs = total % 60;
    if hrs > 0 {
        format!("{}:{:02}:{:02}", hrs, mins, secs)
    } else {
        format!("{:02}:{:02}", mins, secs)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum DisplayMode {
    Fit,
    Fullscreen,
}

impl DisplayMode {
    fn label(&self) -> &'static str {
        match self {
            DisplayMode::Fit => "fit",
            DisplayMode::Fullscreen => "full",
        }
    }
}

fn compute_display_dims(
    img_w: u32,
    img_h: u32,
    mode: DisplayMode,
    cols: u16,
    rows: u16,
) -> (u32, u32) {
    let (cell_w, cell_h) = terminal_cell_size();
    let usable_rows = rows.saturating_sub(1);
    let term_pw = cols as u32 * cell_w;
    let term_ph = usable_rows as u32 * cell_h;

    if img_w == 0 || img_h == 0 {
        return (1, 1);
    }

    match mode {
        DisplayMode::Fit => {
            if img_w <= term_pw && img_h <= term_ph {
                (img_w, img_h)
            } else {
                let scale = (term_pw as f64 / img_w as f64).min(term_ph as f64 / img_h as f64);
                (
                    ((img_w as f64 * scale).round() as u32).max(1),
                    ((img_h as f64 * scale).round() as u32).max(1),
                )
            }
        }
        DisplayMode::Fullscreen => {
            let scale = (term_pw as f64 / img_w as f64).min(term_ph as f64 / img_h as f64);
            (
                ((img_w as f64 * scale).round() as u32).max(1),
                ((img_h as f64 * scale).round() as u32).max(1),
            )
        }
    }
}

fn encode_for_display(
    img: &DynamicImage,
    mode: DisplayMode,
    cols: u16,
    rows: u16,
    max_colors: usize,
) -> (Vec<u8>, u32, u32) {
    let (dw, dh) = compute_display_dims(img.width(), img.height(), mode, cols, rows);
    let resized = if dw != img.width() || dh != img.height() {
        img.resize(dw, dh, image::imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    let sixel = sixel::encode(&resized.to_rgba8(), max_colors);
    (sixel, dw, dh)
}

fn center_offset(img_pw: u32, img_ph: u32, cols: u16, rows: u16) -> (u16, u16) {
    let (cell_w, cell_h) = terminal_cell_size();
    let usable_rows = rows.saturating_sub(1);
    let img_cols = ((img_pw + cell_w - 1) / cell_w).min(cols as u32) as u16;
    let img_rows = ((img_ph + cell_h - 1) / cell_h).min(usable_rows as u32) as u16;
    let col = cols.saturating_sub(img_cols) / 2;
    let row = usable_rows.saturating_sub(img_rows) / 2;
    (col, row)
}

fn display_sixel<W: Write>(
    stdout: &mut W,
    sixel: &[u8],
    img_pw: u32,
    img_ph: u32,
    cols: u16,
    rows: u16,
    status: Option<&str>,
) -> Result<()> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"\x1b[H\x1b[2J");

    if let Some(text) = status {
        let _ = write!(&mut buf, "\x1b[{};1H\x1b[2K{}", rows, text);
    }

    let (col, row) = center_offset(img_pw, img_ph, cols, rows);
    let _ = write!(&mut buf, "\x1b[{};{}H", row + 1, col + 1);

    stdout.write_all(&buf)?;
    stdout.flush()?;

    if !sixel.is_empty() {
        stdout.write_all(sixel)?;
    }
    stdout.flush()?;
    Ok(())
}

struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn show_image<W: Write>(
    stdout: &mut W,
    path: &Path,
    max_colors: usize,
) -> Result<ViewerAction> {
    let img = image::open(path)?;

    let mut mode = DisplayMode::Fit;
    let (cols, rows) = terminal::size()?;
    let (sixel, pw, ph) = encode_for_display(&img, mode, cols, rows, max_colors);
    let status = format!(
        "{}x{} [{}] │ f toggle │ \u{2191}\u{2193} File │ Esc Back",
        img.width(),
        img.height(),
        mode.label()
    );
    display_sixel(stdout, &sixel, pw, ph, cols, rows, Some(&status))?;

    let action = loop {
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Char('f') => {
                        mode = if mode == DisplayMode::Fit {
                            DisplayMode::Fullscreen
                        } else {
                            DisplayMode::Fit
                        };
                        let (cols, rows) = terminal::size()?;
                        let (sixel, pw, ph) =
                            encode_for_display(&img, mode, cols, rows, max_colors);
                        let status = format!(
                            "{}x{} [{}] │ f toggle │ \u{2191}\u{2193} File │ Esc Back",
                            img.width(),
                            img.height(),
                            mode.label()
                        );
                        display_sixel(stdout, &sixel, pw, ph, cols, rows, Some(&status))?;
                    }
                    KeyCode::Up | KeyCode::Char('k') => break ViewerAction::PreviousFile,
                    KeyCode::Down | KeyCode::Char('j') => break ViewerAction::NextFile,
                    KeyCode::Char('q') | KeyCode::Esc => break ViewerAction::ReturnToBrowser,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break ViewerAction::QuitProgram;
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {
                    let (cols, rows) = terminal::size()?;
                    let (sixel, pw, ph) = encode_for_display(&img, mode, cols, rows, max_colors);
                    let status = format!(
                        "{}x{} [{}] │ f toggle │ \u{2191}\u{2193} File │ Esc Back",
                        img.width(),
                        img.height(),
                        mode.label()
                    );
                    display_sixel(stdout, &sixel, pw, ph, cols, rows, Some(&status))?;
                }
                _ => {}
            }
        }
    };

    Ok(action)
}

fn show_video<W: Write>(
    stdout: &mut W,
    path: &Path,
    max_colors: usize,
) -> Result<ViewerAction> {
    let duration = ffmpeg::get_duration(path)?;

    let n_frames = 101;
    let timestamps: Vec<f64> = (0..n_frames)
        .map(|i| {
            let t = duration * i as f64 / (n_frames - 1) as f64;
            if t == 0.0 && duration > 0.1 {
                0.1
            } else {
                t.min(duration - 0.01)
            }
        })
        .collect();

    let first = ffmpeg::extract_frame(path, timestamps[0])?;

    let frames: Arc<Mutex<Vec<Option<DynamicImage>>>> =
        Arc::new(Mutex::new(vec![None; n_frames]));
    frames.lock().unwrap()[0] = Some(first);

    let ready = Arc::new(AtomicUsize::new(1));
    let done = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));

    ffmpeg::spawn_frame_extractor(
        path,
        timestamps.clone(),
        Arc::clone(&frames),
        Arc::clone(&ready),
        Arc::clone(&done),
        Arc::clone(&cancel),
    );

    let mut current: usize = 0;
    let mut mode = DisplayMode::Fit;
    let mut last_ready = 0usize;
    let mut rendered_frame: bool;

    render_video_frame(
        stdout,
        &frames,
        current,
        mode,
        max_colors,
        n_frames,
        &timestamps,
        duration,
        &ready,
        &done,
    )?;
    rendered_frame = true;

    let action = loop {
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    let prev = current;
                    let prev_mode = mode;
                    match key.code {
                        KeyCode::Right | KeyCode::Char('l') => {
                            current = (current + 1).min(n_frames - 1);
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            current = current.saturating_sub(1);
                        }
                        KeyCode::Tab => {
                            current = (current + 10).min(n_frames - 1);
                        }
                        KeyCode::Char(' ') => {
                            current = n_frames - 1;
                        }
                        KeyCode::BackTab | KeyCode::Backspace => {
                            current = current.saturating_sub(10);
                        }
                        KeyCode::Home => {
                            current = 0;
                        }
                        KeyCode::End => {
                            current = n_frames - 1;
                        }
                        KeyCode::Char('f') => {
                            mode = if mode == DisplayMode::Fit {
                                DisplayMode::Fullscreen
                            } else {
                                DisplayMode::Fit
                            };
                        }
                        KeyCode::Up | KeyCode::Char('k') => break ViewerAction::PreviousFile,
                        KeyCode::Down | KeyCode::Char('j') => break ViewerAction::NextFile,
                        KeyCode::Char('q') | KeyCode::Esc => break ViewerAction::ReturnToBrowser,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break ViewerAction::QuitProgram;
                        }
                        _ => continue,
                    }
                    if current != prev || mode != prev_mode {
                        render_video_frame(
                            stdout,
                            &frames,
                            current,
                            mode,
                            max_colors,
                            n_frames,
                            &timestamps,
                            duration,
                            &ready,
                            &done,
                        )?;
                        rendered_frame = true;
                    }
                }
                Event::Resize(_, _) => {
                    render_video_frame(
                        stdout,
                        &frames,
                        current,
                        mode,
                        max_colors,
                        n_frames,
                        &timestamps,
                        duration,
                        &ready,
                        &done,
                    )?;
                    rendered_frame = true;
                }
                _ => {}
            }
        } else {
            let r = ready.load(Ordering::Relaxed);
            if r == last_ready {
                continue;
            }
            last_ready = r;
            let guard = frames.lock().unwrap();
            if guard[current].is_some() {
                drop(guard);
                if !rendered_frame {
                    render_video_frame(
                        stdout,
                        &frames,
                        current,
                        mode,
                        max_colors,
                        n_frames,
                        &timestamps,
                        duration,
                        &ready,
                        &done,
                    )?;
                    rendered_frame = true;
                } else {
                    let status = video_status(
                        current, n_frames, &timestamps, duration, mode, &ready, &done,
                    );
                    update_status_line(stdout, &status)?;
                }
            } else {
                let load_pct = r * 100 / n_frames;
                let status = if done.load(Ordering::Relaxed) {
                    format!("  (frame {} unavailable)", current)
                } else {
                    format!("  loading frame {}... ({}%)", current, load_pct)
                };
                drop(guard);
                update_status_line(stdout, &status)?;
            }
        }
    };

    cancel.store(true, Ordering::Relaxed);
    Ok(action)
}

fn update_status_line<W: Write>(stdout: &mut W, text: &str) -> Result<()> {
    let (_, rows) = terminal::size()?;
    let mut buf = Vec::with_capacity(text.len() + 16);
    let _ = write!(&mut buf, "\x1b[{};1H\x1b[2K{}", rows, text);
    stdout.write_all(&buf)?;
    stdout.flush()?;
    Ok(())
}

fn video_status(
    current: usize,
    n: usize,
    timestamps: &[f64],
    duration: f64,
    mode: DisplayMode,
    ready: &AtomicUsize,
    done: &AtomicBool,
) -> String {
    let progress = if n > 1 {
        current as f64 / (n - 1) as f64 * 100.0
    } else {
        100.0
    };
    let ts = timestamps[current];
    let bar_w = 20usize;
    let filled = (progress / 100.0 * bar_w as f64).round() as usize;
    let bar: String = "\u{2588}".repeat(filled) + &"\u{2591}".repeat(bar_w - filled);
    let load_pct = ready.load(Ordering::Relaxed) * 100 / n;
    let load_tag = if done.load(Ordering::Relaxed) {
        String::new()
    } else {
        format!(" \u{2502} loading {}%", load_pct)
    };
    format!(
        "[{}/{}] {:.0}% {} {} / {} [{}]{} │ \u{2190}\u{2192} \u{00b1}1 │ Tab +10 │ \u{232b} -10 │ Space End │ \u{2191}\u{2193} File │ Esc Back",
        current,
        n - 1,
        progress,
        bar,
        format_time(ts),
        format_time(duration),
        mode.label(),
        load_tag,
    )
}

fn render_video_frame<W: Write>(
    stdout: &mut W,
    frames: &Arc<Mutex<Vec<Option<DynamicImage>>>>,
    current: usize,
    mode: DisplayMode,
    max_colors: usize,
    n: usize,
    timestamps: &[f64],
    duration: f64,
    ready: &AtomicUsize,
    done: &AtomicBool,
) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let guard = frames.lock().unwrap();
    match &guard[current] {
        Some(img) => {
            let (sixel, pw, ph) = encode_for_display(img, mode, cols, rows, max_colors);
            let status = video_status(current, n, timestamps, duration, mode, ready, done);
            drop(guard);
            display_sixel(stdout, &sixel, pw, ph, cols, rows, Some(&status))?;
        }
        None => {
            let load_pct = ready.load(Ordering::Relaxed) * 100 / n;
            let status = if done.load(Ordering::Relaxed) {
                format!("  (frame {} unavailable)", current)
            } else {
                format!("  loading frame {}... ({}%)", current, load_pct)
            };
            drop(guard);
            display_sixel(stdout, &[], 0, 0, cols, rows, Some(&status))?;
        }
    }
    Ok(())
}

#[derive(Clone)]
struct BrowserEntry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

fn load_entries(dir: &Path) -> Vec<BrowserEntry> {
    let mut entries = Vec::new();
    if let Some(parent) = dir.parent() {
        entries.push(BrowserEntry {
            path: parent.to_path_buf(),
            name: "..".to_string(),
            is_dir: true,
        });
    }

    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let is_dir = path.is_dir();
            if is_dir || is_video(&path) || is_image(&path) {
                entries.push(BrowserEntry {
                    path,
                    name: entry.file_name().to_string_lossy().into_owned(),
                    is_dir,
                });
            }
        }
    }

    entries.sort_by(|a, b| {
        if a.name == ".." {
            return std::cmp::Ordering::Less;
        }
        if b.name == ".." {
            return std::cmp::Ordering::Greater;
        }
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

fn show_browser<W: Write>(stdout: &mut W, start_path: &Path, max_colors: usize) -> Result<()> {
    let mut cwd = start_path
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    if cwd.is_file() {
        cwd.pop();
    }

    let mut selected = 0;
    let mut scroll = 0;
    let mut needs_refresh = true;
    let mut entries = Vec::new();

    loop {
        if needs_refresh {
            entries = load_entries(&cwd);
            if selected >= entries.len() {
                selected = entries.len().saturating_sub(1);
            }
            needs_refresh = false;
        }

        let (cols, rows) = terminal::size()?;
        let list_rows = rows.saturating_sub(2).max(1) as usize;

        if selected < scroll {
            scroll = selected;
        }
        if selected >= scroll + list_rows {
            scroll = selected.saturating_sub(list_rows - 1);
        }

        let mut buf = Vec::with_capacity(4096);
        let _ = write!(&mut buf, "\x1b[H\x1b[2J");
        let header = format!(" Browser: {} ", cwd.display());
        let _ = write!(
            &mut buf,
            "\x1b[1;1H\x1b[7m{:<width$}\x1b[0m\x1b[K\r\n",
            header,
            width = cols as usize
        );

        for i in 0..list_rows {
            let idx = scroll + i;
            if idx < entries.len() {
                let entry = &entries[idx];
                let type_tag = if entry.is_dir {
                    "DIR"
                } else if is_video(&entry.path) {
                    "VID"
                } else {
                    "IMG"
                };

                if idx == selected {
                    let _ = write!(&mut buf, "\x1b[7m> [{}] {} \x1b[0m\x1b[K\r\n", type_tag, entry.name);
                } else {
                    let _ = write!(&mut buf, "  [{}] {} \x1b[K\r\n", type_tag, entry.name);
                }
            } else {
                let _ = write!(&mut buf, "\x1b[K\r\n");
            }
        }

        let footer = " \u{2191}\u{2193} Navigate │ Enter View │ Backspace Up │ Esc Quit ";
        let _ = write!(
            &mut buf,
            "\x1b[{};1H\x1b[7m{:<width$}\x1b[0m\x1b[K",
            rows,
            footer,
            width = cols as usize
        );

        stdout.write_all(&buf)?;
        stdout.flush()?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if selected + 1 < entries.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = entries.len().saturating_sub(1),
                    KeyCode::PageUp => selected = selected.saturating_sub(list_rows),
                    KeyCode::PageDown => {
                        selected = (selected + list_rows).min(entries.len().saturating_sub(1))
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        if !entries.is_empty() {
                            let entry = &entries[selected];
                            if entry.is_dir {
                                cwd = entry.path.clone();
                                selected = 0;
                                scroll = 0;
                                needs_refresh = true;
                            } else {
                                let mut current_idx = selected;
                                loop {
                                    let view_entry = &entries[current_idx];
                                    let action_res = if is_video(&view_entry.path) {
                                        show_video(stdout, &view_entry.path, max_colors)
                                    } else if is_image(&view_entry.path) {
                                        show_image(stdout, &view_entry.path, max_colors)
                                    } else {
                                        Ok(ViewerAction::ReturnToBrowser)
                                    };

                                    let action = match action_res {
                                        Ok(a) => a,
                                        Err(e) => {
                                            let _ = update_status_line(stdout, &format!("\x1b[7m\x1b[31m Error: {} \x1b[0m\x1b[K", e));
                                            std::thread::sleep(Duration::from_secs(2));
                                            ViewerAction::ReturnToBrowser
                                        }
                                    };

                                    match action {
                                        ViewerAction::QuitProgram => return Ok(()),
                                        ViewerAction::ReturnToBrowser => {
                                            selected = current_idx;
                                            needs_refresh = true;
                                            break;
                                        }
                                        ViewerAction::NextFile => {
                                            let mut next_idx = current_idx;
                                            for i in (current_idx + 1)..entries.len() {
                                                if !entries[i].is_dir {
                                                    next_idx = i;
                                                    break;
                                                }
                                            }
                                            if next_idx == current_idx {
                                                selected = current_idx;
                                                needs_refresh = true;
                                                break;
                                            }
                                            current_idx = next_idx;
                                        }
                                        ViewerAction::PreviousFile => {
                                            let mut prev_idx = current_idx;
                                            for i in (0..current_idx).rev() {
                                                if !entries[i].is_dir {
                                                    prev_idx = i;
                                                    break;
                                                }
                                            }
                                            if prev_idx == current_idx {
                                                selected = current_idx;
                                                needs_refresh = true;
                                                break;
                                            }
                                            current_idx = prev_idx;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                        if let Some(parent) = cwd.parent() {
                            let prev_dir_name = cwd
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            cwd = parent.to_path_buf();

                            entries = load_entries(&cwd);
                            selected = entries
                                .iter()
                                .position(|e| e.name == prev_dir_name)
                                .unwrap_or(0);

                            scroll = 0;
                            needs_refresh = false;
                        }
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

fn main() -> Result<()> {
    ffmpeg::init()?;

    let cli = Cli::parse();
    let path = Path::new(&cli.path);

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = TermGuard;

    if path.is_file() {
        if is_video(path) {
            let _ = show_video(&mut stdout, path, cli.colors);
        } else {
            let _ = show_image(&mut stdout, path, cli.colors);
        }
    } else {
        show_browser(&mut stdout, path, cli.colors)?;
    }

    Ok(())
}
