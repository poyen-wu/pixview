mod ffmpeg;
mod sixel;

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
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

fn is_video(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    VIDEO_EXTENSIONS.contains(&ext.as_str())
}

fn is_image(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    IMAGE_EXTENSIONS.contains(&ext.as_str())
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
        let _ = write!(
            &mut buf,
            "\x1b[{};1H\x1b[7m{:<width$}\x1b[0m\x1b[K",
            rows,
            text,
            width = cols as usize
        );
    }

    let (col, row) = center_offset(img_pw, img_ph, cols, rows);
    let _ = write!(&mut buf, "\x1b[{};{}H", row + 1, col + 1);

    stdout.write_all(&buf)?;
    stdout.flush()?;

    if !sixel.is_empty() {
        stdout.write_all(sixel)?;
    }
    
    // Move cursor away from the image to the bottom right corner
    let _ = write!(stdout, "\x1b[{};{}H", rows, cols);
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

#[derive(Clone, Debug, PartialEq)]
enum EntryPath {
    Native(PathBuf),
    InZip(PathBuf, String),
}

fn stream_zip_video(archive: PathBuf, name: String) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop_signal = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_signal);
    listener.set_nonblocking(true).unwrap();

    let mut total_size = 0;
    let mut physical_data_start: Option<u64> = None;
    let mut in_memory_cache: Option<Arc<Vec<u8>>> = None;

    if let Ok(file) = std::fs::File::open(&archive) {
        if let Ok(mut zip) = zip::ZipArchive::new(file) {
            if let Ok(mut zfile) = zip.by_name(&name) {
                total_size = zfile.size();
                let is_stored = zfile.compression() == zip::CompressionMethod::Stored;

                if is_stored {
                    if let Some(ds) = zfile.data_start() {
                        physical_data_start = Some(ds);
                        // HARD CAP to prevent EOF crash if ZIP header size is padded
                        if let Ok(meta) = std::fs::metadata(&archive) {
                            let max_available = meta.len().saturating_sub(ds);
                            total_size = total_size.min(max_available);
                        }
                    }
                }

                // Cache compressed files to RAM
                if physical_data_start.is_none() {
                    let mut buf = Vec::with_capacity(total_size as usize);
                    let _ = std::io::copy(&mut zfile, &mut buf);
                    total_size = buf.len() as u64; 
                    in_memory_cache = Some(Arc::new(buf));
                }
            }
        }
    }

    std::thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            if let Ok((mut stream, _)) = listener.accept() {
                // CRITICAL FIX: The accepted stream inherits the listener's non-blocking flag on Unix.
                // We MUST make it blocking, otherwise it drops instantly and causes FFmpeg to 
                // enter an infinite, 100% CPU reconnect loop.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

                let archive = archive.clone();
                let cache = in_memory_cache.clone();

                std::thread::spawn(move || {
                    let mut req_buf = Vec::new();
                    let mut buf = [0; 1024];
                    while let Ok(n) = stream.read(&mut buf) {
                        if n == 0 { break; }
                        req_buf.extend_from_slice(&buf[..n]);
                        if req_buf.windows(4).any(|w| w == b"\r\n\r\n") { break; }
                    }

                    let req_str = String::from_utf8_lossy(&req_buf);
                    if req_str.is_empty() || total_size == 0 { return; }

                    let mut start_byte = 0;
                    let mut end_byte_opt = None;
                    let mut has_range = false;

                    for line in req_str.lines() {
                        let line_clean = line.to_lowercase().replace(" ", "");
                        if line_clean.starts_with("range:bytes=") {
                            has_range = true;
                            if let Some(bytes_str) = line_clean.split("bytes=").nth(1) {
                                let range_str = bytes_str.trim();
                                if range_str.starts_with('-') {
                                    if let Ok(suffix_len) = range_str[1..].parse::<u64>() {
                                        start_byte = total_size.saturating_sub(suffix_len);
                                        end_byte_opt = Some(total_size.saturating_sub(1));
                                    }
                                } else {
                                    let parts: Vec<&str> = range_str.split('-').collect();
                                    if let Ok(b) = parts[0].parse::<u64>() { start_byte = b; }
                                    if parts.len() > 1 && !parts[1].is_empty() {
                                        if let Ok(b) = parts[1].parse::<u64>() { end_byte_opt = Some(b); }
                                    }
                                }
                            }
                        }
                    }

                    if start_byte >= total_size {
                        let headers = format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nConnection: close\r\n\r\n", total_size);
                        let _ = stream.write_all(headers.as_bytes());
                        return;
                    }

                    let end_byte = end_byte_opt.unwrap_or(total_size.saturating_sub(1)).min(total_size.saturating_sub(1));
                    let content_length = end_byte.saturating_sub(start_byte) + 1;

                    let headers = if has_range {
                        format!("HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", start_byte, end_byte, total_size, content_length)
                    } else {
                        format!("HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", total_size)
                    };

                    if stream.write_all(headers.as_bytes()).is_ok() {
                        if let Some(mem_buf) = &cache {
                            let start_idx = start_byte as usize;
                            let end_idx = (start_byte + content_length) as usize;
                            if start_idx <= mem_buf.len() {
                                let safe_end = end_idx.min(mem_buf.len());
                                let _ = stream.write_all(&mem_buf[start_idx..safe_end]);
                            }
                        } else if let Some(ds) = physical_data_start {
                            if let Ok(mut raw_file) = std::fs::File::open(&archive) {
                                if raw_file.seek(SeekFrom::Start(ds + start_byte)).is_ok() {
                                    let mut chunk = raw_file.take(content_length);
                                    let _ = std::io::copy(&mut chunk, &mut stream);
                                }
                            }
                        }
                    }
                });
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });

    (format!("http://127.0.0.1:{}/vid.mp4", port), stop_signal)
}

fn show_image<W: Write>(
    stdout: &mut W,
    path: &EntryPath,
    max_colors: usize,
) -> Result<ViewerAction> {
    let img = match path {
        EntryPath::Native(p) => image::open(p)?,
        EntryPath::InZip(arc, name) => {
            let file = fs::File::open(arc)?;
            let mut zip = zip::ZipArchive::new(file)?;
            let mut zfile = zip.by_name(name)?;
            let mut buf = Vec::new();
            zfile.read_to_end(&mut buf)?;
            image::load_from_memory(&buf)?
        }
    };

    let mut mode = DisplayMode::Fit;
    let (cols, rows) = terminal::size()?;
    let (sixel, pw, ph) = encode_for_display(&img, mode, cols, rows, max_colors);
    let status = format!(
        " {}x{} [{}] │ f toggle │ \u{2191}\u{2193} File │ Esc Back ",
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
                            " {}x{} [{}] │ f toggle │ \u{2191}\u{2193} File │ Esc Back ",
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
                        " {}x{} [{}] │ f toggle │ \u{2191}\u{2193} File │ Esc Back ",
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
    path: &EntryPath,
    max_colors: usize,
) -> Result<ViewerAction> {
    let (vid_path_str, stop_server) = match path {
        EntryPath::Native(p) => (p.to_string_lossy().to_string(), None),
        EntryPath::InZip(arc, name) => {
            let (url, stop) = stream_zip_video(arc.clone(), name.clone());
            (url, Some(stop))
        }
    };

    let duration = ffmpeg::get_duration(&vid_path_str)?;

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

    let first = ffmpeg::extract_frame(&vid_path_str, timestamps[0])?;

    let frames: Arc<Mutex<Vec<Option<DynamicImage>>>> =
        Arc::new(Mutex::new(vec![None; n_frames]));
    frames.lock().unwrap()[0] = Some(first);

    let ready = Arc::new(AtomicUsize::new(1));
    let done = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));

    ffmpeg::spawn_frame_extractor(
        vid_path_str.clone(),
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
            let mut state_changed = false;
            let mut exit_action = None;
            let prev_current = current;
            let prev_mode = mode;

            // Drain the entire event queue to merge sequential input events
            loop {
                match event::read()? {
                    Event::Key(key) => {
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
                            KeyCode::Up | KeyCode::Char('k') => {
                                exit_action = Some(ViewerAction::PreviousFile);
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                exit_action = Some(ViewerAction::NextFile);
                            }
                            KeyCode::Char('q') | KeyCode::Esc => {
                                exit_action = Some(ViewerAction::ReturnToBrowser);
                            }
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                exit_action = Some(ViewerAction::QuitProgram);
                            }
                            _ => {}
                        }
                    }
                    Event::Resize(_, _) => {
                        state_changed = true;
                    }
                    _ => {}
                }

                if exit_action.is_some() {
                    break;
                }

                // If no more events are immediately queued, exit the inner processing loop
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }

            if let Some(action) = exit_action {
                break action;
            }

            if current != prev_current || mode != prev_mode || state_changed {
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
                    format!("  (frame {} unavailable) ", current)
                } else {
                    format!("  loading frame {}... {:>3}% ", current, load_pct)
                };
                drop(guard);
                update_status_line(stdout, &status)?;
            }
        }
    };

    cancel.store(true, Ordering::Relaxed);
    if let Some(stop) = stop_server {
        stop.store(true, Ordering::Relaxed);
    }
    
    Ok(action)
}

fn update_status_line<W: Write>(stdout: &mut W, text: &str) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let mut buf = Vec::with_capacity(text.len() + 32);
    let _ = write!(
        &mut buf,
        "\x1b[{};1H\x1b[7m{:<width$}\x1b[0m\x1b[K\x1b[{};{}H",
        rows,
        text,
        rows,
        cols,
        width = cols as usize
    );
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
        format!(" \u{2502} loading {:>3}%", load_pct)
    };
    
    // Calculates the required padding for frame text (e.g. 100 -> 3 chars)
    let digits = (n.saturating_sub(1)).to_string().len().max(1);

    format!(
        " [{:>w$}/{}] {:>3.0}% {} {} / {} [{}]{} │ \u{2190}\u{2192} \u{00b1}1 │ Tab +10 │ \u{232b} -10 │ Space End │ \u{2191}\u{2193} File │ Esc Back ",
        current,
        n - 1,
        progress,
        bar,
        format_time(ts),
        format_time(duration),
        mode.label(),
        load_tag,
        w = digits,
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
                format!("  (frame {} unavailable) ", current)
            } else {
                format!("  loading frame {}... {:>3}% ", current, load_pct)
            };
            drop(guard);
            display_sixel(stdout, &[], 0, 0, cols, rows, Some(&status))?;
        }
    }
    Ok(())
}

#[derive(Clone)]
struct BrowserEntry {
    path: EntryPath,
    name: String,
    is_dir: bool,
}

fn load_entries(cwd: &EntryPath) -> Vec<BrowserEntry> {
    let mut entries = Vec::new();

    match cwd {
        EntryPath::Native(dir) => {
            if let Some(parent) = dir.parent() {
                entries.push(BrowserEntry {
                    path: EntryPath::Native(parent.to_path_buf()),
                    name: "..".to_string(),
                    is_dir: true,
                });
            }

            if let Ok(rd) = fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let is_zip = name.to_lowercase().ends_with(".zip");
                    let is_dir = path.is_dir() || is_zip;

                    if is_dir || is_video(&name) || is_image(&name) {
                        let ep = if is_zip {
                            EntryPath::InZip(path, String::new())
                        } else {
                            EntryPath::Native(path)
                        };
                        entries.push(BrowserEntry { path: ep, name, is_dir });
                    }
                }
            }
        }
        EntryPath::InZip(archive, prefix) => {
            if prefix.is_empty() {
                if let Some(parent) = archive.parent() {
                    entries.push(BrowserEntry {
                        path: EntryPath::Native(parent.to_path_buf()),
                        name: "..".to_string(),
                        is_dir: true,
                    });
                }
            } else {
                let mut parts: Vec<&str> = prefix.trim_end_matches('/').split('/').collect();
                parts.pop();
                let parent_prefix = if parts.is_empty() {
                    String::new()
                } else {
                    format!("{}/", parts.join("/"))
                };
                entries.push(BrowserEntry {
                    path: EntryPath::InZip(archive.clone(), parent_prefix),
                    name: "..".to_string(),
                    is_dir: true,
                });
            }

            if let Ok(file) = fs::File::open(archive) {
                if let Ok(mut zip) = zip::ZipArchive::new(file) {
                    let mut seen_dirs = HashSet::new();
                    for i in 0..zip.len() {
                        if let Ok(zfile) = zip.by_index(i) {
                            let name = zfile.name();
                            if !name.starts_with(prefix) || name == prefix {
                                continue;
                            }

                            let remainder = &name[prefix.len()..];
                            if let Some(slash_idx) = remainder.find('/') {
                                let dir_name = &remainder[..slash_idx];
                                if !seen_dirs.contains(dir_name) {
                                    seen_dirs.insert(dir_name.to_string());
                                    entries.push(BrowserEntry {
                                        path: EntryPath::InZip(
                                            archive.clone(),
                                            format!("{}{}/", prefix, dir_name),
                                        ),
                                        name: dir_name.to_string(),
                                        is_dir: true,
                                    });
                                }
                            } else {
                                // Exclude nested ZIPs directly.
                                if name.to_lowercase().ends_with(".zip") {
                                    continue;
                                }
                                if is_video(remainder) || is_image(remainder) {
                                    entries.push(BrowserEntry {
                                        path: EntryPath::InZip(
                                            archive.clone(),
                                            name.to_string(),
                                        ),
                                        name: remainder.to_string(),
                                        is_dir: false,
                                    });
                                }
                            }
                        }
                    }
                }
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

/// Recursively fast-forwards through directories that only contain a single subfolder
/// and no other relevant image/video files.
fn resolve_single_dir(mut target: EntryPath) -> EntryPath {
    loop {
        let sub_entries = load_entries(&target);
        let mut real_count = 0;
        let mut only_dir = None;

        for entry in sub_entries {
            if entry.name != ".." {
                real_count += 1;
                if entry.is_dir {
                    only_dir = Some(entry.path);
                }
            }
        }

        if real_count == 1 && only_dir.is_some() {
            target = only_dir.unwrap();
        } else {
            break;
        }
    }
    target
}

fn show_browser<W: Write>(stdout: &mut W, start_path: EntryPath, max_colors: usize) -> Result<()> {
    let mut cwd = start_path;
    let mut selected = 0;
    let mut scroll = 0;
    let mut needs_refresh = true;
    let mut needs_redraw = true;
    let mut entries = Vec::new();

    loop {
        if needs_refresh {
            entries = load_entries(&cwd);
            if selected >= entries.len() {
                selected = entries.len().saturating_sub(1);
            }
            needs_refresh = false;
            needs_redraw = true;
        }

        let (cols, rows) = terminal::size()?;
        let list_rows = rows.saturating_sub(2).max(1) as usize;

        if selected < scroll {
            scroll = selected;
            needs_redraw = true;
        }
        if selected >= scroll + list_rows {
            scroll = selected.saturating_sub(list_rows - 1);
            needs_redraw = true;
        }

        if needs_redraw {
            let mut buf = Vec::with_capacity(4096);
            let _ = write!(&mut buf, "\x1b[H\x1b[2J");
            let header = match &cwd {
                EntryPath::Native(p) => format!(" Browser: {} ", p.display()),
                EntryPath::InZip(arc, prefix) => format!(" Browser: {}/{} ", arc.display(), prefix),
            };
            
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
                    } else if is_video(&entry.name) {
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

            let footer = " \u{2191}\u{2193} Nav │ Enter View │ \u{2190}/h Up │ Tab/Bksp Pg │ Space Last │ q Quit ";
            let _ = write!(
                &mut buf,
                "\x1b[{};1H\x1b[7m{:<width$}\x1b[0m\x1b[K\x1b[{};{}H",
                rows,
                footer,
                rows,
                cols,
                width = cols as usize
            );

            stdout.write_all(&buf)?;
            stdout.flush()?;
            needs_redraw = false;
        }

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                        needs_redraw = true;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if selected + 1 < entries.len() {
                            selected += 1;
                            needs_redraw = true;
                        }
                    }
                    KeyCode::Home => {
                        selected = 0;
                        needs_redraw = true;
                    }
                    KeyCode::End | KeyCode::Char(' ') => {
                        if !entries.is_empty() {
                            selected = entries.len().saturating_sub(1);
                            needs_redraw = true;
                        }
                    }
                    KeyCode::PageUp | KeyCode::Backspace => {
                        selected = selected.saturating_sub(list_rows);
                        needs_redraw = true;
                    }
                    KeyCode::PageDown | KeyCode::Tab => {
                        selected = (selected + list_rows).min(entries.len().saturating_sub(1));
                        needs_redraw = true;
                    }
                    KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                        if !entries.is_empty() {
                            let entry = &entries[selected];
                            if entry.is_dir {
                                if entry.name == ".." {
                                    cwd = entry.path.clone();
                                } else {
                                    cwd = resolve_single_dir(entry.path.clone());
                                }
                                selected = 0;
                                scroll = 0;
                                needs_refresh = true;
                                needs_redraw = true;
                            } else {
                                let mut current_idx = selected;
                                loop {
                                    let view_entry = &entries[current_idx];
                                    let action_res = if is_video(&view_entry.name) {
                                        show_video(stdout, &view_entry.path, max_colors)
                                    } else if is_image(&view_entry.name) {
                                        show_image(stdout, &view_entry.path, max_colors)
                                    } else {
                                        Ok(ViewerAction::ReturnToBrowser)
                                    };

                                    let action = match action_res {
                                        Ok(a) => a,
                                        Err(e) => {
                                            let (cols, rows) = terminal::size().unwrap_or((80, 24));
                                            let err_msg = format!(" Error: {} ", e);
                                            let mut err_buf = Vec::new();
                                            let _ = write!(
                                                &mut err_buf,
                                                "\x1b[{};1H\x1b[7m\x1b[31m{:<width$}\x1b[0m\x1b[K\x1b[{};{}H",
                                                rows,
                                                err_msg,
                                                rows,
                                                cols,
                                                width = cols as usize
                                            );
                                            let _ = stdout.write_all(&err_buf);
                                            let _ = stdout.flush();
                                            std::thread::sleep(Duration::from_secs(2));
                                            ViewerAction::ReturnToBrowser
                                        }
                                    };

                                    match action {
                                        ViewerAction::QuitProgram => return Ok(()),
                                        ViewerAction::ReturnToBrowser => {
                                            selected = current_idx;
                                            needs_refresh = true;
                                            needs_redraw = true;
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
                                                needs_redraw = true;
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
                                                needs_redraw = true;
                                                break;
                                            }
                                            current_idx = prev_idx;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        if let Some(parent_entry) = entries.iter().find(|e| e.name == "..").cloned() {
                            let prev_dir_name = match &cwd {
                                EntryPath::Native(p) => p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                                EntryPath::InZip(arc, prefix) => {
                                    if prefix.is_empty() {
                                        arc.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                                    } else {
                                        let trimmed = prefix.trim_end_matches('/');
                                        trimmed.split('/').last().unwrap_or("").to_string()
                                    }
                                }
                            };

                            cwd = parent_entry.path;
                            entries = load_entries(&cwd);
                            
                            // Scan the parent and re-select the folder we just came from
                            selected = entries.iter().position(|e| e.name == prev_dir_name).unwrap_or(0);
                            
                            scroll = 0;
                            needs_refresh = false;
                            needs_redraw = true;
                        }
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {
                    needs_redraw = true;
                }
                _ => {}
            }
        }
    }
}

fn main() -> Result<()> {
    ffmpeg::init()?;

    let cli = Cli::parse();
    
    let path = Path::new(&cli.path);
    let abs_path = path.canonicalize().unwrap_or_else(|_| PathBuf::from(&cli.path));
    
    let is_zip_ext = abs_path.extension().map_or(false, |e| e.eq_ignore_ascii_case("zip"));
    
    let mut start_path = if abs_path.is_file() {
        if is_zip_ext {
            EntryPath::InZip(abs_path.clone(), String::new())
        } else {
            EntryPath::Native(abs_path)
        }
    } else {
        EntryPath::Native(abs_path)
    };

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = TermGuard;

    if let EntryPath::Native(p) = &mut start_path {
        if p.is_file() && !is_zip_ext {
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
