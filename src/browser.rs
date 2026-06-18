use std::fs;
use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal,
};

use crate::archive::{self, ArchiveType, EntryPath};
use crate::util::{is_image, is_video};
use crate::viewer::{show_image, show_video, ViewerAction};

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
                    let arc_ty = archive::archive_type(&name);
                    let is_dir = path.is_dir() || arc_ty.is_some();

                    if is_dir || is_video(&name) || is_image(&name) {
                        let ep = match arc_ty {
                            Some(ArchiveType::Zip) => EntryPath::InZip(path, String::new()),
                            Some(ArchiveType::Rar) => EntryPath::InRar(path, String::new()),
                            None => EntryPath::Native(path),
                        };
                        entries.push(BrowserEntry { path: ep, name, is_dir });
                    }
                }
            }
        }
        EntryPath::InZip(archive, prefix) | EntryPath::InRar(archive, prefix) => {
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
                let parent_path = match cwd {
                    EntryPath::InZip(a, _) => EntryPath::InZip(a.clone(), parent_prefix),
                    EntryPath::InRar(a, _) => EntryPath::InRar(a.clone(), parent_prefix),
                    _ => unreachable!(),
                };
                entries.push(BrowserEntry {
                    path: parent_path,
                    name: "..".to_string(),
                    is_dir: true,
                });
            }

            if let Ok(a_entries) = archive::list_archive(cwd) {
                for e in a_entries {
                    if !e.is_dir {
                        // Exclude nested archives (zip-in-zip, rar-in-rar, ...)
                        if archive::archive_type(&e.display_name).is_some() {
                            continue;
                        }
                        if !is_video(&e.display_name) && !is_image(&e.display_name) {
                            continue;
                        }
                    }
                    let ep = match cwd {
                        EntryPath::InZip(a, _) => EntryPath::InZip(a.clone(), e.internal_path),
                        EntryPath::InRar(a, _) => EntryPath::InRar(a.clone(), e.internal_path),
                        _ => unreachable!(),
                    };
                    entries.push(BrowserEntry {
                        path: ep,
                        name: e.display_name,
                        is_dir: e.is_dir,
                    });
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
pub(crate) fn resolve_single_dir(mut target: EntryPath) -> EntryPath {
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

pub fn show_browser<W: Write>(stdout: &mut W, start_path: EntryPath, max_colors: usize) -> Result<()> {
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
                EntryPath::InRar(arc, prefix) => format!(" Browser: {}/{} ", arc.display(), prefix),
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
                                EntryPath::InZip(arc, prefix) | EntryPath::InRar(arc, prefix) => {
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
