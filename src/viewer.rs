use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal,
};
use image::DynamicImage;

use crate::archive::{self, EntryPath};
use crate::{ffmpeg, sixel};

pub enum ViewerAction {
    QuitProgram,
    ReturnToBrowser,
    NextFile,
    PreviousFile,
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

pub fn show_image<W: Write>(
    stdout: &mut W,
    path: &EntryPath,
    max_colors: usize,
) -> Result<ViewerAction> {
    let img = match path {
        EntryPath::Native(p) => image::open(p)?,
        _ => {
            let buf = archive::read_entry(path)?;
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

pub fn show_video<W: Write>(
    stdout: &mut W,
    path: &EntryPath,
    max_colors: usize,
) -> Result<ViewerAction> {
    let (vid_path_str, stop_server) = match path {
        EntryPath::Native(p) => (p.to_string_lossy().to_string(), None),
        _ => {
            let (url, stop) = archive::stream_video(path).unwrap();
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
