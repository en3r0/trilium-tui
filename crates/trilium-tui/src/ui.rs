use crate::app::{App, Confirm, Focus, Kind, Overlay, Picker, PickerKind, Prompt, SyncState, ViewMode};
use crate::{highlight, keymap::Action, render};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use unicode_width::UnicodeWidthStr;

pub fn draw(f: &mut Frame, app: &mut App) {
    let show_tabs = app.tabs.len() > 1;
    let [tabbar, main, status] = Layout::vertical([Constraint::Length(u16::from(show_tabs)), Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
    if show_tabs { draw_tabs(f, app, tabbar); }
    let narrow = main.width < 80;
    let (tree_r, note_r) = if narrow {
        match app.focus {
            Focus::Tree => (main, Rect::default()),
            Focus::Note => (Rect::default(), main),
        }
    } else {
        let tw = ((u32::from(main.width) * 30 / 100) as u16).clamp(24, 48);
        let [t, n] = Layout::horizontal([Constraint::Length(tw), Constraint::Min(20)]).areas(main);
        (t, n)
    };
    app.tree_area = tree_r;
    app.note_area = note_r;
    if tree_r.width > 0 { draw_tree(f, app, tree_r); }
    if note_r.width > 0 { draw_note(f, app, note_r); }
    draw_status(f, app, status);
    if let Overlay::Image(p) = &mut app.overlay { draw_image_overlay(f, p, f.area()); return; }
    if let Overlay::Edit(s) = &mut app.overlay { app.edit_area = f.area(); crate::edit_view::draw(f, s, f.area()); return; }
    match &app.overlay {
        Overlay::Image(_) | Overlay::Edit(_) => {}
        Overlay::Picker(p) => draw_picker(f, app, p, f.area()),
        Overlay::Help => draw_help(f, app, f.area()),
        Overlay::Prompt(p) => draw_prompt(f, p, f.area()),
        Overlay::Confirm(c) => draw_confirm(f, c, f.area()),
        Overlay::Panel(p) => draw_panel(f, app, p, f.area()),
        Overlay::None => {}
    }
}

fn focus_style(focused: bool) -> Style {
    if focused { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::DarkGray) }
}

fn draw_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let title = if app.hoisted == "root" { " Notes ".to_string() } else { format!(" ⌂ {} ", app.model.title_of(&app.hoisted)) };
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(focus_style(app.focus == Focus::Tree)).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let h = inner.height as usize;
    app.tree_height = h;
    if app.sel < app.tree_scroll { app.tree_scroll = app.sel; }
    if h > 0 && app.sel >= app.tree_scroll + h { app.tree_scroll = app.sel + 1 - h; }
    let focused = app.focus == Focus::Tree;
    let lines: Vec<Line> = app.rows.iter().enumerate().skip(app.tree_scroll).take(h).map(|(i, r)| {
        let marker = if !r.expandable { "  " } else if r.expanded { "▾ " } else { "▸ " };
        let mut spans = vec![Span::raw("  ".repeat(r.depth)), Span::styled(marker, Style::default().fg(Color::DarkGray)), Span::raw(r.label.clone())];
        if r.cloned { spans.push(Span::styled(" ⧉", Style::default().fg(Color::DarkGray))); }
        if app.model.notes.get(&r.note_id).is_some_and(|n| n.protected) { spans.push(Span::styled(" 🔒", Style::default().fg(Color::Yellow))); }
        let mut line = Line::from(spans);
        if app.is_marked(&r.branch_id) { line = line.style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)); }
        if i == app.sel {
            line = line.style(if focused { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default().add_modifier(Modifier::BOLD) });
        }
        line
    }).collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// Looks up an included note's title and HTML for embedding in a rendered view.
fn include_resolver(app: &App) -> impl Fn(&str) -> Option<(String, String)> + '_ {
    move |id: &str| {
        let n = app.model.notes.get(id)?;
        if n.ntype != "text" { return None; } // a locked protected note yields no content below
        Some((n.title.clone(), app.db.note_content(id).ok().flatten()?))
    }
}

fn build_html(app: &App, width: usize) -> render::Rendered {
    let v = app.view.as_ref().unwrap();
    let resolver = include_resolver(app);
    if matches!(v.kind, Kind::Code) {
        let html = render::markdown_to_html(&v.content, &|id| app.model.notes.get(id).map(|n| n.title.clone()));
        return render::render_html_full(&html, width, &resolver);
    }
    render::render_html_full(&v.content, width, &resolver)
}

fn build_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(v) = app.view.as_ref() else { return vec![Line::from("No note selected")] };
    let note = &app.model.notes[&v.id];
    let dim = Style::default().fg(Color::DarkGray);
    match (&v.kind, v.mode) {
        (Kind::Html, ViewMode::Rendered) => unreachable!("rendered HTML goes through build_html"),
        (Kind::Html, ViewMode::Source) => highlight::hard_wrap(highlight::highlight(&v.content, "text/html"), width),
        (Kind::Code, _) => highlight::hard_wrap(highlight::highlight(&v.content, &note.mime), width),
        (Kind::Unsupported(t), _) => {
            let mut out = vec![
                Line::from(Span::styled(format!("⚠ '{t}' notes need the browser renderer; showing the stored source."), Style::default().fg(Color::Yellow))),
                Line::from(Span::styled("  Press o to open this note in the browser.", dim)),
                Line::default(),
            ];
            out.extend(highlight::hard_wrap(highlight::highlight(&v.content, &note.mime), width));
            out
        }
        (Kind::Collection, _) => app.collection_lines(&v.id, width),
        (Kind::SearchResults(ids), _) => {
            let mut out = vec![Line::from(vec![Span::styled("Saved search: ", dim), Span::styled(v.content.clone(), Style::default().fg(Color::Cyan)), Span::styled(format!("   {} result(s)", ids.len()), dim)]), Line::default()];
            for id in ids {
                let path = app.model.path_to(id).map(|p| p[..p.len().saturating_sub(1)].iter().map(|i| app.model.title_of(i)).collect::<Vec<_>>().join(" / ")).unwrap_or_default();
                out.push(Line::from(vec![Span::raw("▸ "), Span::raw(app.model.title_of(id).to_string()), Span::styled(format!("   {path}"), dim)]));
            }
            out
        }
        (Kind::Image, _) => vec![Line::from("(picture)")],
        (Kind::Binary, _) => vec![
            Line::from(format!("{} note", note.ntype)),
            Line::from(Span::styled(format!("mime: {}", note.mime), dim)),
            Line::from(Span::styled(format!("size: {} bytes", v.size), dim)),
            Line::default(),
            Line::from("Binary content isn't shown in the terminal."),
            Line::from(vec![Span::styled("  x", Style::default().fg(Color::Cyan)), Span::raw("  save it to your Downloads folder")]),
            Line::from(vec![Span::styled("  O", Style::default().fg(Color::Cyan)), Span::raw("  open it with your default application")]),
            Line::from(vec![Span::styled("  o", Style::default().fg(Color::Cyan)), Span::raw("  open the note in the browser")]),
        ],
        (Kind::Protected, _) => vec![
            Line::from(Span::styled("🔒 Protected note — locked; title and content are encrypted.", Style::default().fg(Color::Yellow))),
            Line::default(),
            Line::from("Unlock it from the command palette: \"Unlock protected notes\"."),
        ],
    }
}

fn draw_note(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Note;
    let (title, badge) = match app.view.as_ref() {
        Some(v) if app.model.notes.contains_key(&v.id) => {
            let n = &app.model.notes[&v.id];
            let mode = if matches!(v.kind, Kind::Html | Kind::Code) && n.ntype != "" && (matches!(v.kind, Kind::Html) || crate::render::is_markdown_mime(&n.mime)) { if v.mode == ViewMode::Source { " source" } else { "" } } else { "" };
            (n.title.clone(), format!("{}{mode}", n.ntype))
        }
        _ => (String::new(), String::new()),
    };
    let editing = app.rich.as_ref().filter(|_| matches!(app.overlay, Overlay::None) || matches!(app.overlay, Overlay::Help));
    let block = match editing {
        Some(r) => {
            let accent = Style::default().fg(Color::Cyan);
            let modified = if r.dirty() { Span::styled(" ● ", Style::default().fg(Color::Yellow)) } else { Span::raw(" ") };
            Block::bordered().border_type(BorderType::Rounded).border_style(accent)
                .title(Line::from(vec![Span::styled(" ✎ ", accent), Span::styled(r.title.clone(), Style::default().add_modifier(Modifier::BOLD)), modified]))
        }
        None => Block::bordered().border_type(BorderType::Rounded).border_style(focus_style(focused))
            .title(Line::from(vec![Span::raw(format!(" {title} ")), Span::styled(format!("[{badge}] "), Style::default().fg(Color::DarkGray))])),
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    let has_attrs = app.view.as_ref().is_some_and(|v| !v.attrs.is_empty());
    let [content_r, attr_r] = Layout::vertical([Constraint::Min(1), Constraint::Length(u16::from(has_attrs))]).areas(inner);
    let content_r = Rect { x: content_r.x + 1, width: content_r.width.saturating_sub(2), ..content_r };
    app.note_height = content_r.height as usize;
    let width = content_r.width as usize;
    if app.rich.is_some() && matches!(app.overlay, Overlay::None | Overlay::Help) {
        if let Some(mut r) = app.rich.take() {
            r.draw(f, content_r, &include_resolver(app));
            app.rich = Some(r);
        }
        draw_attr_strip(f, app, attr_r);
        return;
    }
    if app.view.as_ref().is_some_and(|v| matches!(v.kind, Kind::Image)) {
        app.ensure_note_picture();
        if let Some(pic) = app.view.as_mut().and_then(|v| v.picture.as_deref_mut()) { draw_picture(f, pic, content_r, true); }
        draw_attr_strip(f, app, attr_r);
        return;
    }
    if let Some(_vt) = app.custom_collection() {
        let lines = app.custom_collection_lines(width, content_r.height as usize);
        f.render_widget(Paragraph::new(lines), content_r);
        draw_attr_strip(f, app, attr_r);
        return;
    }
    let stale = app.view.as_ref().is_some_and(|v| v.cache.as_ref().is_none_or(|c| c.0 != width || c.1 != v.mode));
    if stale {
        let html_view = app.view.as_ref().is_some_and(|v| v.mode == ViewMode::Rendered && (matches!(v.kind, Kind::Html) || (matches!(v.kind, Kind::Code) && app.model.notes.get(&v.id).is_some_and(|n| crate::render::is_markdown_mime(&n.mime)))));
        if html_view {
            let r = build_html(app, width);
            if let Some(v) = app.view.as_mut() {
                let keep = v.sel_link.filter(|i| *i < r.links.len());
                v.links = r.links; v.link_spans = r.span_links; v.sel_link = keep;
                v.cache = Some((width, v.mode, r.lines));
            }
        } else {
            let lines = build_lines(app, width);
            if let Some(v) = app.view.as_mut() { v.links.clear(); v.link_spans.clear(); v.sel_link = None; v.cache = Some((width, v.mode, lines)); }
        }
    }
    let focused_note = app.focus == Focus::Note;
    if let Some(v) = app.view.as_mut() {
        let total = v.cache.as_ref().map_or(0, |c| c.2.len());
        let is_list = matches!(v.kind, Kind::Collection | Kind::SearchResults(_));
        let h = content_r.height as usize;
        if is_list {
            let row = 2 + v.sel; // two header lines precede the entries
            if row < v.scroll { v.scroll = row.saturating_sub(2); }
            if h > 0 && row >= v.scroll + h { v.scroll = row + 1 - h; }
        }
        v.scroll = v.scroll.min(total.saturating_sub(1));
        if let Some((_, _, lines)) = &v.cache {
            let sel_link = v.sel_link.map(|i| i as u32);
            let visible: Vec<Line> = lines.iter().enumerate().skip(v.scroll).take(h).map(|(i, l)| {
                if is_list && i == 2 + v.sel {
                    l.clone().style(if focused_note { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default().add_modifier(Modifier::BOLD) })
                } else if sel_link.is_some() {
                    let mut l = l.clone();
                    if let Some(ids) = v.link_spans.get(i) {
                        for (span, id) in l.spans.iter_mut().zip(ids) {
                            if *id == sel_link { span.style = span.style.add_modifier(Modifier::REVERSED | Modifier::BOLD); }
                        }
                    }
                    l
                } else { l.clone() }
            }).collect();
            f.render_widget(Paragraph::new(visible), content_r);
        }
    }
    if has_attrs { draw_attr_strip(f, app, attr_r); }
    if let Some(v) = app.view.as_ref() {
        let total = v.cache.as_ref().map_or(0, |c| c.2.len());
        let h = content_r.height as usize;
        if total > h && h > 0 {
            let mut st = ScrollbarState::new(total - h).position(v.scroll.min(total - h)).viewport_content_length(h);
            let sb = Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None).track_symbol(None).thumb_symbol("┃").thumb_style(focus_style(focused));
            f.render_stateful_widget(sb, Rect { y: content_r.y, height: content_r.height, ..area }, &mut st);
        }
    }
}

fn draw_attr_strip(f: &mut Frame, app: &App, attr_r: Rect) {
    if attr_r.height == 0 { return; }
    if let Some(v) = app.view.as_ref() {
        let mut spans = Vec::new();
        for a in &v.attrs {
            let (sigil, color) = if a.kind == "relation" { ("~", Color::Magenta) } else { ("#", Color::Green) };
            let val = if a.kind == "relation" { format!("={}", app.model.title_of(&a.value)) } else if a.value.is_empty() { String::new() } else { format!("={}", a.value) };
            spans.push(Span::styled(format!("{sigil}{}{val}  ", a.name), Style::default().fg(color)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), Rect { x: attr_r.x + 1, width: attr_r.width.saturating_sub(2), ..attr_r });
    }
}

/// Draw a picture fitted into `area` (with a one-line caption when `caption`), or why it can't be shown.
fn draw_picture(f: &mut Frame, pic: &mut crate::images::Picture, area: Rect, caption: bool) {
    let dim = Style::default().fg(Color::DarkGray);
    let [cap_r, img_r] = if caption { Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area) } else { [Rect { height: 0, ..area }, area] };
    if caption {
        let hint = format!("{}  {}x{}  {}   Enter zoom · x save · O open externally", pic.filename, pic.dims.0, pic.dims.1, pic.mime);
        f.render_widget(Paragraph::new(Line::from(Span::styled(hint, dim))), cap_r);
    }
    match (&mut pic.proto, &pic.error) {
        (Some(proto), _) => {
            f.render_stateful_widget(ratatui_image::StatefulImage::default(), img_r, proto);
            if let Some(Err(e)) = proto.last_encoding_result() {
                f.render_widget(Paragraph::new(Line::from(Span::styled(format!("could not draw the image: {e}"), Style::default().fg(Color::Red)))), img_r);
            }
        }
        (None, Some(e)) => f.render_widget(Paragraph::new(vec![Line::from(Span::styled(format!("⚠ {e}"), Style::default().fg(Color::Yellow)))]), img_r),
        _ => {}
    }
}

fn draw_image_overlay(f: &mut Frame, pic: &mut crate::images::Picture, area: Rect) {
    f.render_widget(Clear, area);
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::default().fg(Color::Cyan)).title(format!(" {} ", pic.title));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [img_r, foot_r] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    draw_picture(f, pic, img_r, false);
    let foot = format!("{}  {}x{}  {}   x save · o open externally · any other key closes", pic.filename, pic.dims.0, pic.dims.1, pic.mime);
    f.render_widget(Paragraph::new(Line::from(Span::styled(foot, Style::default().fg(Color::DarkGray)))), foot_r);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let (sync, sync_color) = match &app.sync {
        SyncState::Off => ("local only".to_string(), Color::DarkGray),
        SyncState::Syncing => ("⟳ syncing".to_string(), Color::Yellow),
        SyncState::Ok(t, pushed) => {
            let s = t.elapsed().as_secs();
            let up = if *pushed > 0 { format!(" ↑{pushed}") } else { String::new() };
            (if s < 5 { format!("✓ synced{up}") } else { format!("✓ synced {s}s ago{up}") }, Color::Green)
        }
        SyncState::Error(e) => (format!("✗ {}", e.chars().take(40).collect::<String>()), Color::Red),
    };
    let pct = app.view.as_ref().and_then(|v| v.cache.as_ref().map(|c| (v.scroll, c.2.len()))).map(|(s, t)| if t == 0 { 100 } else { ((s + app.note_height).min(t)) * 100 / t }).unwrap_or(0);
    let dim = Style::default().fg(Color::DarkGray);
    let lock = if app.db.has_password() { if app.db.is_unlocked() { "🔓 " } else { "🔒 " } } else { "" };
    let right_text = format!("{lock}{pct}%  {sync}  F1 help ");
    let w = area.width as usize;
    let lw = w.saturating_sub(right_text.width() + 2);
    let mut left_spans: Vec<Span> = vec![Span::raw(" ")];
    if app.status.is_empty() {
        let path: Vec<String> = app.view.as_ref().and_then(|v| app.model.path_to(&v.id)).map(|p| p.iter().map(|i| app.model.title_of(i).to_string()).collect()).unwrap_or_default();
        let mut used = 0usize;
        for (i, seg) in path.iter().enumerate() {
            let last = i + 1 == path.len();
            let sep = if i == 0 { "" } else { "  " };
            let room = lw.saturating_sub(used + sep.width());
            if room == 0 { break; }
            // cut by characters first (a title can be hundreds of thousands of characters: popping one by one is quadratic)
            let mut t: String = seg.chars().take(room).collect();
            while t.width() > room { t.pop(); }
            used += sep.width() + t.width();
            if !sep.is_empty() { left_spans.push(Span::styled(sep, dim)); }
            left_spans.push(Span::styled(t, if last { Style::default().add_modifier(Modifier::BOLD) } else { dim }));
        }
    } else {
        let mut l: String = app.status.chars().take(lw).collect();
        while l.width() > lw { l.pop(); }
        left_spans.push(Span::styled(l, Style::default().fg(Color::Cyan)));
    }
    let lused: usize = left_spans.iter().map(|s| s.content.width()).sum();
    let pad = w.saturating_sub(lused + right_text.width());
    left_spans.push(Span::raw(" ".repeat(pad)));
    left_spans.push(Span::styled(format!("{lock}{pct}%  "), dim));
    left_spans.push(Span::styled(sync, Style::default().fg(sync_color)));
    left_spans.push(Span::styled("  F1 help ", dim));
    f.render_widget(Paragraph::new(Line::from(left_spans)), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width); let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 3, width: w, height: h }
}

fn draw_picker(f: &mut Frame, app: &App, p: &Picker, area: Rect) {
    let r = centered(area, 80, (area.height * 2 / 3).max(8));
    f.render_widget(Clear, r);
    let title = match p.kind { PickerKind::Jump => " Jump to note ", PickerKind::Search => " Search (Enter to run) ", PickerKind::Command => " Command palette ", PickerKind::CloneTo => " Clone to… (pick the parent) ", PickerKind::MoveTo => " Move to… (pick the parent) " };
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::default().fg(Color::Cyan)).title(title);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [input_r, list_r] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    f.render_widget(Paragraph::new(vec![Line::from(vec![Span::styled("> ", Style::default().fg(Color::Cyan)), Span::raw(p.query.clone()), Span::styled("▏", Style::default().fg(Color::Cyan))]), Line::default()]), input_r);
    let h = list_r.height as usize;
    let top = p.sel.saturating_sub(h.saturating_sub(1));
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line> = Vec::new();
    if let Some(e) = &p.error { lines.push(Line::from(Span::styled(format!("⚠ {e}"), Style::default().fg(Color::Red)))); }
    if p.results.is_empty() {
        let msg = match (p.kind, p.query.trim().is_empty(), p.executed) {
            (PickerKind::Command, ..) => "No matching command",
            (PickerKind::Search, true, _) => "Type a query, then Enter. Trilium syntax works: #label, #year >= 2020, ~relation.title = x, note.title *=* x, orderBy",
            (PickerKind::Search, false, false) => "Press Enter to search titles and content",
            _ => "No matches",
        };
        lines.push(Line::from(Span::styled(msg, dim)));
    }
    if p.kind == PickerKind::Command {
        for (i, id) in p.results.iter().enumerate().skip(top).take(h) {
            let Some((a, t)) = Action::ALL.iter().find(|(a, _)| format!("{a:?}") == *id) else { continue };
            let keys = app.keymap.labels_for(*a).join(", ");
            let mut line = Line::from(vec![Span::raw(t.to_string()), Span::styled(format!("   {keys}"), dim)]);
            if i == p.sel { line = line.style(Style::default().add_modifier(Modifier::REVERSED)); }
            lines.push(line);
        }
        f.render_widget(Paragraph::new(lines), list_r);
        return;
    }
    for (i, id) in p.results.iter().enumerate().skip(top).take(h) {
        let title = app.model.title_of(id).to_string();
        let path = app.model.path_to(id).map(|p| p[..p.len().saturating_sub(1)].iter().map(|i| app.model.title_of(i)).collect::<Vec<_>>().join(" / ")).unwrap_or_default();
        let mut line = Line::from(vec![Span::raw(title), Span::styled(format!("   {path}"), dim)]);
        if i == p.sel { line = line.style(Style::default().add_modifier(Modifier::REVERSED)); }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), list_r);
}

fn first_key(app: &App, a: Action) -> String { app.keymap.labels_for(a).into_iter().next().unwrap_or_else(|| "(unbound)".into()) }

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let r = centered(area, 104, area.height.saturating_sub(4).max(10));
    f.render_widget(Clear, r);
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::default().fg(Color::Cyan)).title(" Help — ↑↓ scroll · any other key closes ");
    let inner = block.inner(r);
    f.render_widget(block, r);
    let dim = Style::default().fg(Color::DarkGray);
    let h = |s: &str| Line::from(Span::styled(s.to_string(), Style::default().add_modifier(Modifier::BOLD)));
    let row = |keys: String, what: &str| {
        let mut k = keys;
        while k.width() > 30 { k.pop(); }
        Line::from(vec![Span::styled(format!("  {k:<30}"), Style::default().fg(Color::Cyan)), Span::raw(what.to_string())])
    };
    let km = |a: Action| { let l = app.keymap.labels_for(a); if l.is_empty() { "(unbound)".into() } else { l.join(", ") } };
    let lines = vec![
        h("Same keys as the Trilium web app"),
        row(km(Action::JumpToNote), "jump to note (fuzzy by title)"),
        row(km(Action::QuickSearch), "search titles and content"),
        row(km(Action::Back) + " / " + &km(Action::Forward), "note history back / forward"),
        row(km(Action::ScrollToActiveNote), "reveal open note in the tree"),
        row(km(Action::CollapseTree), "collapse the whole tree"),
        row(km(Action::CollapseSubtree), "collapse selected subtree"),
        row(km(Action::ToggleArchived), "show/hide archived notes"),
        row(km(Action::Hoist) + " / " + &km(Action::Unhoist), "hoist selected note / unhoist"),
        row(km(Action::SyncNow), "sync now"),
        row(km(Action::NewTab) + " / " + &km(Action::CloseTab), "new tab / close tab"),
        row(first_key(app, Action::NextTab) + " / " + &first_key(app, Action::PrevTab), "next / previous tab"),
        row("Ctrl+1 … Ctrl+9".into(), "go to tab (9 = last)"),
        row(km(Action::CommandPalette) + ", :", "command palette"),
        row(format!("{}, i", km(Action::ShowAttributes)), "note panel: attributes, backlinks, revisions, info, attachments"),
        row("b / v (tree)".into(), "backlinks / revisions of the selected note"),
        row(km(Action::CreateNoteAfter), "create note after the selected one"),
        row(km(Action::CreateNoteInto), "create child note"),
        row(km(Action::EditTitle), "rename (tree)"),
        row(km(Action::EditPrefix), "edit branch prefix (tree)"),
        row(km(Action::DeleteNotes), "delete note / remove clone (tree)"),
        row(km(Action::MoveUp) + " / " + &km(Action::MoveDown), "reorder (tree)"),
        row(format!("m, {} / {}", km(Action::SelectAbove), km(Action::SelectBelow)), "mark notes for multi-select (Esc clears, Ctrl+A all siblings)"),
        row(format!("{} {} {}", km(Action::CopyNotes), km(Action::CutNotes), km(Action::PasteNotes)), "copy / cut / paste (copy+paste clones, cut+paste moves)"),
        row(format!("{} / {}", km(Action::CloneTo), km(Action::MoveTo)), "clone / move marked notes to a note you pick"),
        row(km(Action::MoveOut) + " / " + &km(Action::MoveIn), "move out of / into previous sibling (tree)"),
        row(km(Action::AddLabel) + " / " + &km(Action::AddRelation), "add label / relation"),
        row("e".into(), "edit the note in place (formatted, no tags); other notes open the source editor"),
        row("Ctrl+e".into(), "edit the stored HTML/Markdown/code source in the built-in editor (lossless)"),
        row("E".into(), "edit it in $VISUAL / $EDITOR instead"),
        row("(in place)".into(), "^S save · Esc close · ^Z/^Y undo · ^B bold · ^U underline · Alt+I (or ^I) italic · Alt+S strike · Alt+C code · Alt+H mark"),
        row("(in place)".into(), "^K link · ^F find · F3 next · Alt+1..6 heading · Alt+0 paragraph · Alt+U/O/T bullet/numbered/to-do list · Alt+Q quote"),
        row("(in place)".into(), "Tab / Shift+Tab nest / unnest list item · Ctrl+Space or click toggles a to-do · Shift/Alt+Enter line break · F2 HTML source"),
        row("(in place)".into(), "type `- `, `1. `, `# `, `> `, `[ ] ` at the start of a paragraph to start a list, heading, quote or to-do"),
        row("(source)".into(), "^S save · Esc close · ^F find · F3 next · ^G line · ^Z/^Y undo · ^K cut line · ^D dup line · F2 edit in place (HTML notes)"),
        Line::default(),
        h("Tree"),
        row("↑ ↓  k j".into(), "move"),
        row("→ l / ← h".into(), "expand / collapse (←: go to parent)"),
        row("Space".into(), "toggle expand"),
        row("t".into(), "open today's journal note"),
        row("g / G, PgUp / PgDn".into(), "first / last, page"),
        Line::default(),
        h("Note"),
        row("↑ ↓  k j, PgUp PgDn".into(), "scroll"),
        row("s".into(), "rendered <-> HTML source (text notes)"),
        row("] / [  then Enter".into(), "select next / previous link, follow it (Ctrl+Enter too)"),
        row("o".into(), "open in the browser (types the TUI can't render)"),
        row("x / O".into(), "save / open externally the file or image the note holds"),
        row("a (note) / c (tree)".into(), "append a line to this note / capture a line into today's note"),
        row("collections".into(), "list/table: ↑↓ Enter · board: h/l j/k, H/L move card, n new · calendar: arrows, [ ] month, t, Enter, n"),
        row("Enter".into(), "zoom an image note; on a selected image link, view it"),
        row("Esc / h".into(), "back to the tree"),
        Line::default(),
        h("Anywhere"),
        row("Tab".into(), "switch pane"),
        row("/".into(), "search"),
        row("q, Ctrl+Q".into(), "quit"),
        Line::default(),
        Line::from(Span::styled(format!("{} of {} bound Trilium actions are implemented so far.", app.keymap.implemented(), app.keymap.total_actions), dim)),
        Line::from(Span::styled("Override keys in ~/.config/trilium-tui/keys.json (Trilium action names).", dim)),
    ];
    let max = lines.len().saturating_sub(inner.height as usize);
    let scroll = app.help_scroll.get().min(max);
    app.help_scroll.set(scroll);
    f.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner });
    if max > 0 {
        let mut st = ScrollbarState::new(max).position(scroll).viewport_content_length(inner.height as usize);
        f.render_stateful_widget(Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None).track_symbol(None).thumb_symbol("┃").thumb_style(Style::default().fg(Color::Cyan)), r, &mut st);
    }
}

fn draw_prompt(f: &mut Frame, p: &Prompt, area: Rect) {
    let r = centered(area, 70, 4);
    f.render_widget(Clear, r);
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::default().fg(Color::Cyan)).title(format!(" {} ", p.label));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let shown: Vec<char> = if p.secret { vec!['•'; p.text.len()] } else { p.text.clone() };
    let before: String = shown[..p.cursor].iter().collect();
    let at: String = shown.get(p.cursor).map(|c| c.to_string()).unwrap_or_else(|| " ".into());
    let after: String = shown.iter().skip(p.cursor + 1).collect();
    let line = Line::from(vec![Span::raw(before), Span::styled(at, Style::default().add_modifier(Modifier::REVERSED)), Span::raw(after)]);
    f.render_widget(Paragraph::new(vec![line, Line::from(Span::styled("Enter to confirm, Esc to cancel", Style::default().fg(Color::DarkGray)))]), Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner });
}

fn draw_confirm(f: &mut Frame, c: &Confirm, area: Rect) {
    let r = centered(area, (c.message.width() as u16 + 6).clamp(30, 90), 4);
    f.render_widget(Clear, r);
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::default().fg(Color::Red)).title(" Confirm ");
    let inner = block.inner(r);
    f.render_widget(block, r);
    f.render_widget(Paragraph::new(vec![Line::from(c.message.clone()), Line::from(Span::styled("y = yes, any other key = cancel", Style::default().fg(Color::DarkGray)))]), Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner });
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut spans: Vec<Span> = Vec::new();
    for (i, (title, active)) in app.tab_titles().into_iter().enumerate() {
        let mut t: String = title.chars().take(22).collect();
        if title.chars().count() > 22 { t.push('…'); }
        let st = if active { Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { Style::default().fg(Color::DarkGray) };
        spans.push(Span::styled(format!(" {} {t} ", i + 1), st));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_panel(f: &mut Frame, app: &App, p: &crate::panel::Panel, area: Rect) {
    use crate::panel::PanelTab;
    let r = centered(area, area.width.saturating_sub(6).min(120), area.height.saturating_sub(4).max(10));
    f.render_widget(Clear, r);
    let title = format!(" {} ", app.model.title_of(&p.note_id));
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::default().fg(Color::Cyan)).title(title);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [tabs_r, body_r, foot_r] = Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let dim = Style::default().fg(Color::DarkGray);
    // tab bar
    let mut spans: Vec<Span> = vec![];
    for (i, t) in PanelTab::ALL.iter().enumerate() {
        let count = p.count(*t).filter(|c| *c > 0).map(|c| format!(" {c}")).unwrap_or_default();
        let st = if *t == p.tab { Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { dim };
        spans.push(Span::styled(format!(" {} {}{} ", i + 1, t.title(), count), st));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), tabs_r);
    let body = Rect { x: body_r.x + 1, width: body_r.width.saturating_sub(2), ..body_r };
    let h = body.height as usize;
    let top = p.sel.saturating_sub(h.saturating_sub(1));
    let sel_style = Style::default().add_modifier(Modifier::REVERSED);
    let mut lines: Vec<Line> = vec![];
    let foot: String;
    if let Some(d) = &p.detail {
        let src = if d.show_text { &d.text } else { &d.diff };
        lines.push(Line::from(vec![Span::styled(format!("Revision \"{}\"  ", d.title), Style::default().add_modifier(Modifier::BOLD)), Span::styled(if d.show_text { "(revision text)" } else { "(diff: revision -> current)" }, dim)]));
        for l in src.iter().skip(d.scroll).take(h.saturating_sub(1)) { lines.push(l.clone()); }
        foot = "↑↓ scroll  v diff/text  r restore this revision  Esc back".into();
    } else {
        match p.tab {
            PanelTab::Attributes => {
                if p.attrs.is_empty() { lines.push(Line::from(Span::styled("No attributes. Press a (label) or A (relation) to add one.", dim))); }
                for (i, a) in p.attrs.iter().enumerate().skip(top).take(h) {
                    let (sigil, color) = if a.kind == "relation" { ("~", Color::Magenta) } else { ("#", Color::Green) };
                    let val = if a.kind == "relation" { format!("= {}", app.model.title_of(&a.value)) } else if a.value.is_empty() { String::new() } else { format!("= {}", a.value) };
                    let origin = if a.owner != p.note_id { format!("   inherited from {}", app.model.title_of(&a.owner)) } else if a.inheritable { "   (inheritable)".to_string() } else { String::new() };
                    let mut l = Line::from(vec![Span::styled(format!("{sigil}{} ", a.name), Style::default().fg(color)), Span::raw(val), Span::styled(origin, dim)]);
                    if i == p.sel { l = l.style(sel_style); }
                    lines.push(l);
                }
                foot = "a add label  A add relation  e edit value  d delete  Enter open relation target  Esc close".into();
            }
            PanelTab::Backlinks => {
                if p.backlinks.is_empty() { lines.push(Line::from(Span::styled("No other note links here.", dim))); }
                for (i, b) in p.backlinks.iter().enumerate().skip(top).take(h) {
                    let path = app.model.path_to(&b.source).map(|pp| pp[..pp.len().saturating_sub(1)].iter().map(|x| app.model.title_of(x)).collect::<Vec<_>>().join(" / ")).unwrap_or_default();
                    let kind = if b.is_link { String::new() } else { format!("  ~{}", b.relation) };
                    let mut l = Line::from(vec![Span::raw(app.model.title_of(&b.source).to_string()), Span::styled(kind, Style::default().fg(Color::Magenta)), Span::styled(format!("   {path}"), dim)]);
                    if i == p.sel { l = l.style(sel_style); }
                    lines.push(l);
                }
                foot = "↑↓ select  Enter open the linking note  Esc close".into();
            }
            PanelTab::Revisions => {
                if p.revs.is_empty() { lines.push(Line::from(Span::styled("No revisions yet. They are saved automatically as you edit (at most every 10 minutes), or via the palette.", dim))); }
                for (i, rv) in p.revs.iter().enumerate().skip(top).take(h) {
                    let when = rv.utc_created.get(..16).unwrap_or(&rv.utc_created).replace('T', " ");
                    let mut l = Line::from(vec![Span::raw(format!("{when}  ")), Span::raw(rv.title.clone()), Span::styled(format!("   {}  {} B", rv.source, rv.size), dim),
                        Span::styled(if rv.description.is_empty() { String::new() } else { format!("  “{}”", rv.description) }, Style::default().fg(Color::Yellow))]);
                    if i == p.sel { l = l.style(sel_style); }
                    lines.push(l);
                }
                foot = "↑↓ select  Enter show diff vs current, then r restores  Esc close".into();
            }
            PanelTab::Info => {
                let i = &p.info;
                let paths: Vec<String> = i.parents.iter().map(|par| {
                    let mut path = app.model.path_to(par).unwrap_or_default();
                    if par != "root" { path.push(String::new()); path.pop(); }
                    let mut titles: Vec<String> = path.iter().map(|x| app.model.title_of(x).to_string()).collect();
                    titles.push(app.model.title_of(&i.id).to_string());
                    titles.join(" / ")
                }).collect();
                let kv = |k: &str, v: String| Line::from(vec![Span::styled(format!("{k:<14}"), dim), Span::raw(v)]);
                lines.push(kv("Note ID", i.id.clone()));
                lines.push(kv("Type", format!("{}  ({})", i.ntype, i.mime)));
                lines.push(kv("Created", format!("{}   (UTC {})", i.date_created, i.utc_created)));
                lines.push(kv("Modified", format!("{}   (UTC {})", i.date_modified, i.utc_modified)));
                lines.push(kv("Content", format!("{} bytes   blob {}", i.content_size, i.blob_id)));
                lines.push(kv("Children", i.children.to_string()));
                lines.push(kv("Revisions", i.revisions.to_string()));
                lines.push(kv("Attachments", i.attachments.to_string()));
                lines.push(kv("Protected", i.protected.to_string()));
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(if paths.len() > 1 { "Note paths (cloned)" } else { "Note path" }, dim)));
                for pth in paths { lines.push(Line::from(format!("  {pth}"))); }
                foot = "Tab switch  Esc close".into();
            }
            PanelTab::Attachments => {
                if p.atts.is_empty() { lines.push(Line::from(Span::styled("This note has no attachments.", dim))); }
                for (i, a) in p.atts.iter().enumerate().skip(top).take(h) {
                    let mut l = Line::from(vec![Span::raw(a.title.clone()), Span::styled(format!("   {}  {}  {} B", a.role, a.mime, a.size), dim)]);
                    if i == p.sel { l = l.style(sel_style); }
                    lines.push(l);
                }
                foot = "↑↓ select  Enter / x save to your Downloads folder  Esc close".into();
            }
        }
    }
    f.render_widget(Paragraph::new(lines), body);
    let foot_line = match &p.pending { Some((_, msg)) => Line::from(Span::styled(msg.clone(), Style::default().fg(Color::Red))), None => Line::from(Span::styled(foot, dim)) };
    f.render_widget(Paragraph::new(foot_line), Rect { x: foot_r.x + 1, width: foot_r.width.saturating_sub(2), ..foot_r });
}
