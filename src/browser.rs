use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal,
};

use crate::archive::{self, ArchiveType, EntryClassification, EntryPath};
use crate::util::{is_image, is_video};
use crate::viewer::{show_image, show_video, ViewerAction};

#[derive(Clone)]
struct BrowserEntry {
    path: EntryPath,
    name: String,
    is_dir: bool,
    /// False for non-primary parts of a split archive (e.g. `.002`, `.part02.rar`).
    /// Rendered dimmed and skipped by cursor navigation.
    selectable: bool,
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
                    selectable: true,
                });
            }

            // Classify the whole directory in one pass so split-set grouping
            // (which needs to inspect siblings) only scans once.
            let classifications = archive::classify_directory(dir).unwrap_or_default();

            for (path, class) in classifications {
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();

                let arc_ty = class.archive_type();
                let is_split_nonprimary = matches!(
                    &class,
                    EntryClassification::SplitMember { is_primary: false, .. }
                );
                // Non-primary parts aren't navigable on their own; sort with files.
                let is_dir = !is_split_nonprimary && (path.is_dir() || arc_ty.is_some());

                // Keep split non-primary parts visible (greyed) even though they
                // have no media extension; the cursor skips over them.
                if !(is_dir || is_split_nonprimary || is_video(&name) || is_image(&name)) {
                    continue;
                }

                let selectable = !is_split_nonprimary;

                // Non-selectable parts are never navigated (cursor can't land
                // on them), so the EntryPath variant is moot — use Native.
                let ep = if is_split_nonprimary {
                    EntryPath::Native(path)
                } else {
                    match arc_ty {
                        Some(ArchiveType::Zip) => EntryPath::InZip(path, String::new()),
                        Some(ArchiveType::Rar) => EntryPath::InRar(path, String::new()),
                        Some(ArchiveType::SevenZ) => EntryPath::InSevenZ(path, String::new()),
                        None => EntryPath::Native(path),
                    }
                };
                entries.push(BrowserEntry {
                    path: ep,
                    name,
                    is_dir,
                    selectable,
                });
            }
        }
        EntryPath::InZip(archive, prefix)
        | EntryPath::InRar(archive, prefix)
        | EntryPath::InSevenZ(archive, prefix) => {
            if prefix.is_empty() {
                if let Some(parent) = archive.parent() {
                    entries.push(BrowserEntry {
                        path: EntryPath::Native(parent.to_path_buf()),
                        name: "..".to_string(),
                        is_dir: true,
                        selectable: true,
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
                    EntryPath::InSevenZ(a, _) => EntryPath::InSevenZ(a.clone(), parent_prefix),
                    _ => unreachable!(),
                };
                entries.push(BrowserEntry {
                    path: parent_path,
                    name: "..".to_string(),
                    is_dir: true,
                    selectable: true,
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
                        EntryPath::InSevenZ(a, _) => {
                            EntryPath::InSevenZ(a.clone(), e.internal_path)
                        }
                        _ => unreachable!(),
                    };
                    entries.push(BrowserEntry {
                        path: ep,
                        name: e.display_name,
                        is_dir: e.is_dir,
                        selectable: true,
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

/// Walk forward or backward one entry at a time, skipping non-selectable rows.
/// Returns the resulting index (clamped to bounds). If the destination is
/// non-selectable, keeps walking in the same direction until a selectable row
/// is found or a boundary is hit (in which case the current index is kept).
fn step_selectable(entries: &[BrowserEntry], from: usize, forward: bool) -> usize {
    if entries.is_empty() {
        return 0;
    }
    let mut cur = from.min(entries.len() - 1);
    loop {
        let next = if forward {
            cur.checked_add(1).filter(|&n| n < entries.len())
        } else {
            cur.checked_sub(1)
        };
        match next {
            Some(n) => {
                cur = n;
                if entries[cur].selectable {
                    return cur;
                }
            }
            None => return cur,
        }
    }
}

/// Snap to the nearest selectable entry from `from` (searching both forward
/// and backward). Used after Home/End/Page jumps that may land on a
/// non-selectable row.
fn settle_selectable(entries: &[BrowserEntry], from: usize) -> usize {
    if entries.is_empty() {
        return 0;
    }
    let from = from.min(entries.len() - 1);
    if entries[from].selectable {
        return from;
    }
    for offset in 1..=entries.len() {
        let fwd = from.checked_add(offset).filter(|&n| n < entries.len());
        let bwd = from.checked_sub(offset);
        if let Some(f) = fwd {
            if entries[f].selectable {
                return f;
            }
        }
        if let Some(b) = bwd {
            if entries[b].selectable {
                return b;
            }
        }
    }
    from
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
            // Snap off any non-selectable landing (e.g. after entering a dir
            // whose first row happens to be a non-primary split part).
            selected = settle_selectable(&entries, selected);
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
                EntryPath::InSevenZ(arc, prefix) => {
                    format!(" Browser: {}/{} ", arc.display(), prefix)
                }
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
                    let type_tag = if !entry.selectable {
                        "PT "
                    } else if entry.is_dir {
                        "DIR"
                    } else if is_video(&entry.name) {
                        "VID"
                    } else {
                        "IMG"
                    };

                    if !entry.selectable {
                        // Dim non-selectable (non-primary split parts).
                        let _ = write!(
                            &mut buf,
                            "\x1b[2m  [{}] {} \x1b[0m\x1b[K\r\n",
                            type_tag,
                            entry.name
                        );
                    } else if idx == selected {
                        let _ = write!(
                            &mut buf,
                            "\x1b[7m> [{}] {} \x1b[0m\x1b[K\r\n",
                            type_tag,
                            entry.name
                        );
                    } else {
                        let _ = write!(
                            &mut buf,
                            "  [{}] {} \x1b[K\r\n",
                            type_tag,
                            entry.name
                        );
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
                        selected = step_selectable(&entries, selected, false);
                        needs_redraw = true;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = step_selectable(&entries, selected, true);
                        needs_redraw = true;
                    }
                    KeyCode::Home => {
                        selected = settle_selectable(&entries, 0);
                        needs_redraw = true;
                    }
                    KeyCode::End | KeyCode::Char(' ') => {
                        if !entries.is_empty() {
                            selected = settle_selectable(&entries, entries.len() - 1);
                            needs_redraw = true;
                        }
                    }
                    KeyCode::PageUp | KeyCode::Backspace => {
                        let target = selected.saturating_sub(list_rows);
                        selected = settle_selectable(&entries, target);
                        needs_redraw = true;
                    }
                    KeyCode::PageDown | KeyCode::Tab => {
                        let target = (selected + list_rows).min(entries.len().saturating_sub(1));
                        selected = settle_selectable(&entries, target);
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
                                                if !entries[i].is_dir && entries[i].selectable {
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
                                                if !entries[i].is_dir && entries[i].selectable {
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
                                EntryPath::InZip(arc, prefix)
                                | EntryPath::InRar(arc, prefix)
                                | EntryPath::InSevenZ(arc, prefix) => {
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
