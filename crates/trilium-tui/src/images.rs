//! Pictures: decoding, protocol selection (sixel / kitty / iTerm2 / half-blocks via ratatui-image) and the
//! helpers to save or open files externally.
use anyhow::{Context, Result, bail};
use ratatui_image::{picker::Picker, protocol::StatefulProtocol};

/// Largest side kept after decoding; bigger images are shrunk first (encoding cost grows with pixels).
const MAX_SIDE: u32 = 2400;
const MAX_BYTES: usize = 40 * 1024 * 1024;

pub struct Picture {
    pub title: String,
    pub filename: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    pub dims: (u32, u32),
    pub proto: Option<StatefulProtocol>,
    pub error: Option<String>,
}

impl Picture {
    pub fn load(picker: &Picker, title: &str, filename: &str, mime: &str, bytes: Vec<u8>) -> Picture {
        let mut p = Picture { title: title.into(), filename: filename.into(), mime: mime.into(), dims: (0, 0), proto: None, error: None, bytes };
        match decode(&p.bytes, mime) {
            Ok(img) => { p.dims = (img.width(), img.height()); p.proto = Some(picker.new_resize_protocol(img)); }
            Err(e) => p.error = Some(format!("{e:#}")),
        }
        p
    }
}

fn decode(bytes: &[u8], mime: &str) -> Result<image::DynamicImage> {
    if bytes.len() > MAX_BYTES { bail!("image is {} MB; too large to preview (x saves it, O opens it)", bytes.len() / 1_000_000); }
    if mime.contains("svg") { bail!("SVG can't be previewed in the terminal (x saves it, O opens it in your viewer)"); }
    let mut img = image::load_from_memory(bytes).context("can't decode this image format (x saves it, O opens it)")?;
    if img.width().max(img.height()) > MAX_SIDE { img = img.thumbnail(MAX_SIDE, MAX_SIDE); }
    Ok(img)
}

pub fn is_image_mime(mime: &str) -> bool { mime.starts_with("image/") }

/// A file name that is safe to create in a directory of our choosing.
pub fn safe_name(name: &str, fallback: &str) -> String {
    // path separators, control and bidi/format characters (`a\u{202e}gpj.exe` would show as an .exe spoof) become '_'
    let bidi = |c: char| matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}');
    let s: String = name.chars().map(|c| if c == '/' || c == '\\' || c.is_control() || bidi(c) { '_' } else { c }).collect::<String>().trim().trim_start_matches('.').to_string();
    // file systems allow 255 bytes per name; keep the extension when cutting
    let s = if s.len() > 200 {
        let (stem, ext) = match s.rfind('.') { Some(i) if s.len() - i <= 12 => (&s[..i], &s[i..]), _ => (&s[..], "") };
        let mut end = 200usize.saturating_sub(ext.len()).min(stem.len());
        while !stem.is_char_boundary(end) { end -= 1; }
        format!("{}{ext}", &stem[..end])
    } else { s };
    if s.is_empty() { fallback.into() } else { s }
}

/// Title as a file name: keep an existing extension, otherwise add the one the mime type implies.
pub fn file_name_for(title: &str, mime: &str) -> String {
    let has_ext = title.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()));
    if has_ext { title.to_string() } else { format!("{title}{}", extension_for_mime(mime)) }
}

pub fn extension_for_mime(mime: &str) -> &'static str {
    match mime { "image/png" => ".png", "image/jpeg" | "image/jpg" => ".jpg", "image/gif" => ".gif", "image/webp" => ".webp", "image/svg+xml" => ".svg",
        "application/pdf" => ".pdf", "text/plain" => ".txt", _ => "" }
}

/// Save into the download directory without overwriting an existing file (or following a link someone planted there).
pub fn save_unique(name: &str, bytes: &[u8]) -> Result<std::path::PathBuf> {
    use std::io::Write;
    let dir = std::env::var_os("TRILIUM_TUI_DOWNLOAD_DIR").map(std::path::PathBuf::from)
        .or_else(dirs::download_dir).or_else(dirs::home_dir).unwrap_or_else(|| std::path::PathBuf::from("."));
    let name = safe_name(name, "file");
    let mut n = 0;
    loop {
        let path = if n == 0 { dir.join(&name) } else { dir.join(format!("{n}-{name}")) };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => { f.write_all(bytes).with_context(|| format!("writing {}", path.display()))?; return Ok(path); }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
}

static TEMP_DIRS: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

/// File types the desktop would run rather than show; notes are untrusted, so these are never auto-opened.
fn is_runnable(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    // only types a viewer will show are opened by themselves: no extension, or one we don't know, goes through `x` (save) instead
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "pdf" | "txt" | "md" | "csv" | "json" | "mp3" | "ogg" | "flac" | "wav" | "mp4" | "webm" | "mkv" | "mov" | "zip" | "tar" | "gz" | "odt" | "ods" | "odp" | "docx" | "xlsx" | "pptx" | "epub") { return true; }
    matches!(ext.as_str(), "desktop" | "appimage" | "sh" | "bash" | "zsh" | "fish" | "run" | "bin" | "exe" | "bat" | "cmd" | "com" | "scr" | "jar" | "msi" | "lnk" | "command" | "ps1" | "vbs" | "service" | "timer" | "html" | "htm" | "xhtml" | "svg" | "xml" | "swf")
}

/// Write to a fresh private temp dir and open with the desktop's default application.
pub fn open_externally(name: &str, bytes: &[u8]) -> Result<std::path::PathBuf> {
    let name = safe_name(name, "file");
    if is_runnable(&name) { anyhow::bail!("'{name}' is a runnable file type; save it with x and inspect it before running anything from a note"); }
    // XDG_RUNTIME_DIR is per-user and 0700; /tmp is shared, so only fall back to it with an unpredictable directory name
    let base = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
    let dir = tempfile::Builder::new().prefix("trilium-tui-").tempdir_in(base).context("creating a private temp directory")?.keep();
    TEMP_DIRS.lock().unwrap().push(dir.clone());
    let path = dir.join(&name);
    std::fs::write(&path, bytes)?;
    std::process::Command::new("xdg-open").arg(&path).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().context("xdg-open")?;
    Ok(path)
}

pub fn cleanup_temp() { for d in TEMP_DIRS.lock().unwrap().drain(..) { let _ = std::fs::remove_dir_all(d); } }

/// Choose the image protocol. The terminal query (which finds sixel / kitty / iTerm2 support and the font size)
/// only runs on terminals known to answer it immediately; elsewhere, and inside tmux, pictures are drawn with
/// half-block characters, which work everywhere. `TRILIUM_TUI_IMAGES=halfblocks` forces that, `=query` forces the query.
pub fn make_picker() -> Picker {
    let mode = std::env::var("TRILIUM_TUI_IMAGES").unwrap_or_default();
    let env = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty());
    let term = std::env::var("TERM").unwrap_or_default();
    let known = term.contains("foot") || term.contains("kitty") || term.contains("ghostty") || term.contains("wezterm")
        || env("KITTY_WINDOW_ID") || env("GHOSTTY_RESOURCES_DIR") || env("WEZTERM_EXECUTABLE") || env("ITERM_SESSION_ID") || env("KONSOLE_VERSION");
    let query = match mode.as_str() { "halfblocks" => false, "query" => true, _ => known && !env("TMUX") };
    if query { Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks()) } else { Picker::halfblocks() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::{StatefulImage, picker::ProtocolType};

    fn png() -> Vec<u8> {
        let img = image::RgbImage::from_fn(80, 60, |x, y| image::Rgb([x as u8 * 3, y as u8 * 4, 100]));
        let mut out = std::io::Cursor::new(vec![]);
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn file_names_keep_or_get_extensions() {
        assert_eq!(file_name_for("01.jpeg", "image/jpg"), "01.jpeg");
        assert_eq!(file_name_for("Photo", "image/png"), "Photo.png");
        assert_eq!(file_name_for("v1.2 notes", "text/plain"), "v1.2 notes.txt");
        assert_eq!(safe_name("../../etc/passwd", "f"), "_.._etc_passwd", "path separators can never escape the download directory");
        assert_eq!(safe_name("   ", "f"), "f");
    }

    #[test]
    fn every_graphics_protocol_encodes_without_error() {
        for proto in [ProtocolType::Sixel, ProtocolType::Kitty, ProtocolType::Iterm2, ProtocolType::Halfblocks] {
            let mut picker = Picker::halfblocks();
            picker.set_protocol_type(proto);
            let mut pic = Picture::load(&picker, "t", "t.png", "image/png", png());
            assert!(pic.error.is_none() && pic.dims == (80, 60));
            let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
            term.draw(|f| f.render_stateful_widget(StatefulImage::default(), f.area(), pic.proto.as_mut().unwrap())).unwrap();
            assert!(pic.proto.as_mut().unwrap().last_encoding_result().is_none_or(|r| r.is_ok()), "{proto:?}");
            let buf = term.backend().buffer().clone();
            let all: String = (0..12).flat_map(|y| (0..40).map(move |x| (x, y))).map(|(x, y)| buf[(x, y)].symbol().to_string()).collect();
            match proto {
                ProtocolType::Sixel => assert!(all.contains("\x1bP"), "sixel DCS sequence present"),
                ProtocolType::Kitty => assert!(all.contains("\x1b_G"), "kitty graphics command present"),
                ProtocolType::Iterm2 => assert!(all.contains("\x1b]1337"), "iTerm2 inline image present"),
                _ => assert!(all.contains('▀') || all.contains('▄')),
            }
        }
    }

    #[test]
    fn oversized_and_corrupt_inputs_are_refused_politely() {
        let picker = Picker::halfblocks();
        let svg = Picture::load(&picker, "s", "s.svg", "image/svg+xml", b"<svg/>".to_vec());
        assert!(svg.error.as_deref().unwrap().contains("SVG"));
        let bad = Picture::load(&picker, "b", "b.png", "image/png", vec![1, 2, 3]);
        assert!(bad.error.as_deref().unwrap().contains("can't decode"));
    }
}
