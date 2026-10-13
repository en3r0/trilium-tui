//! "Share" export: a note and its subtree as a self-contained static website (one HTML page per note, a
//! collapsible tree and a title/text search driven by two small scripts). It plays the role of Trilium's
//! share-theme ZIP export without needing the Trilium web app's templates. Protected notes and notes marked
//! `#excludeFromExport` are left out; scripts and event handlers are stripped from every page.
use crate::{
    db::Db,
    htmltree::{decode_entities, escape_attr, escape_text, parse, sanitize_html, text_content},
    transfer::{ExportStats, add_file, extname, sanitize_filename},
    write::is_string_note,
};
use anyhow::{Context, Result, bail};
use regex::Regex;
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::io::{Seek, Write};
use std::path::Path;

struct Page {
    id: String,
    title: String,
    ntype: String,
    mime: String,
    parent: Option<usize>,
    children: Vec<usize>,
    icon: Option<String>,
}

/// Trilium Next's own share-theme stylesheet (AGPL-3.0-or-later, copied from the v0.106.0 server build), so the exported site
/// looks like a shared note on a Trilium server. The page structure below follows its `page.ejs`.
const THEME_CSS: &str = include_str!("share_theme.css");

/// Our additions: attachment list and search-result placement.
const EXTRA_CSS: &str = r#".attachments{margin:1.5rem 0;padding-top:.5rem;border-top:1px solid var(--background-highlight)}
.attachments ul{margin:.3rem 0 0;padding-left:1.2rem}
.search-results{max-height:70vh;overflow:auto;width:min(26rem,90vw)}
.no-results{padding:6px 12px;color:var(--text-menu)}
"#;

const JS: &str = r#"(function(){
var T=window.TREE||[],S=window.SEARCH||[],cur=document.body.getAttribute('data-id'),root=T.length?T[0][0]:'';
var de=document.documentElement,kids={},byId={};
T.forEach(function(n){byId[n[0]]=n;(kids[n[2]||'']=kids[n[2]||'']||[]).push(n)});
function store(k,v){try{localStorage.setItem(k,v)}catch(e){}}
var mobile=function(){return window.matchMedia&&window.matchMedia('(max-width:48em)').matches};
function pane(btn,desktopCls,mobileCls,key){var b=document.getElementById(btn);if(!b)return;
 b.addEventListener('click',function(){if(mobile()){document.body.classList.toggle(mobileCls)}else{var on=de.classList.toggle(desktopCls);store(key,on?'true':'false')}})}
pane('left-pane-toggle-button','left-pane-collapsed','menu-open','left-pane-collapsed');
pane('toc-pane-toggle-button','toc-pane-collapsed','toc-open','toc-pane-collapsed');
var sw=document.querySelector('.theme-selection input');
if(sw){sw.checked=de.classList.contains('theme-dark');sw.addEventListener('change',function(){var t=sw.checked?'dark':'light';de.classList.remove('theme-dark','theme-light');de.classList.add('theme-'+t);store('theme',t)})}
var menu=document.getElementById('menu');
if(menu&&kids[root]){
 var open={};for(var p=cur;p;p=byId[p]&&byId[p][2])open[p]=1;
 var svg='<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="svg-icon right-triangle"><path d="M3 8L12 17L21 8"></path></svg>';
 var item=function(n){var k=kids[n[0]]||[],li=document.createElement('li');li.className=(k.length?'submenu-item':'item')+(open[n[0]]?' expanded':'');
  var a=document.createElement('a');a.className='type-'+(n[3]||'text')+(n[0]===cur?' active':'');a.href=(n[0]===root?'index':n[0])+'.html';
  if(k.length){var b=document.createElement('button');b.className='collapse-button';b.setAttribute('aria-label','Expand');b.innerHTML=svg;
   b.addEventListener('click',function(e){e.preventDefault();e.stopPropagation();li.classList.toggle('expanded')});a.appendChild(b)}
  var sp=document.createElement('span');if(n[4]){var ic=document.createElement('i');ic.className='tn-icon '+n[4];sp.appendChild(ic);sp.appendChild(document.createTextNode(' '))}sp.appendChild(document.createTextNode(n[1]));a.appendChild(sp);li.appendChild(a);
  if(k.length){var ul=document.createElement('ul');k.forEach(function(c){ul.appendChild(item(c))});li.appendChild(ul)}return li};
 var top=document.createElement('ul');kids[root].forEach(function(n){top.appendChild(item(n))});menu.appendChild(top);
}
var box=document.querySelector('.search-input');
if(box){var res=document.createElement('div');res.className='search-results';res.hidden=true;box.parentNode.appendChild(res);
 box.addEventListener('input',function(){var q=box.value.trim().toLowerCase();res.innerHTML='';res.hidden=!q;if(!q)return;var n=0;
  for(var i=0;i<S.length&&n<30;i++){var e=S[i],ax=e[2].toLowerCase().indexOf(q);if(e[1].toLowerCase().indexOf(q)<0&&ax<0)continue;n++;
   var a=document.createElement('a');a.className='search-result-item';a.href=(e[0]===root?'index':e[0])+'.html';
   var t=document.createElement('span');t.className='search-result-note';t.textContent=e[1];a.appendChild(t);
   if(ax>=0){var s=document.createElement('span');s.className='search-result-snippet';var from=Math.max(0,ax-30);
    s.appendChild(document.createTextNode((from?'…':'')+e[2].slice(from,ax)));var m=document.createElement('b');m.textContent=e[2].slice(ax,ax+q.length);s.appendChild(m);
    s.appendChild(document.createTextNode(e[2].slice(ax+q.length,ax+q.length+70)));a.appendChild(s)}
   res.appendChild(a)}
  if(!n){var d=document.createElement('div');d.className='no-results';d.textContent='No results';res.appendChild(d)}});
 document.addEventListener('click',function(e){if(!res.contains(e.target)&&e.target!==box)res.hidden=true})}
var toc=document.getElementById('toc');
if(toc&&window.IntersectionObserver){var links={};toc.querySelectorAll('a').forEach(function(a){links[a.getAttribute('href').slice(1)]=a});
 var io=new IntersectionObserver(function(es){es.forEach(function(e){if(e.isIntersecting){Object.keys(links).forEach(function(k){links[k].classList.remove('active')});var l=links[e.target.id];if(l)l.classList.add('active')}})},{rootMargin:'0px 0px -75% 0px'});
 document.querySelectorAll('#content a.toc-anchor').forEach(function(a){io.observe(a)})}
})();
"#;

/// Trilium's boxicons font and the glyph table for its class names (see `share_icons.css`); note icons follow `getNoteIcon`.
const ICONS_CSS: &str = include_str!("share_icons.css");
const ICONS_FONT: &[u8] = include_bytes!("boxicons.woff2");
const ICONS_NOTICE: &str = "The note icons are Boxicons (https://boxicons.com), free under the Creative Commons Attribution 4.0 licence (https://creativecommons.org/licenses/by/4.0/); the font is the one Trilium Next ships.\n";

const CODE_ICONS: &[(&str, &str)] = &[
    ("text/plain", "bx bx-file"),
    ("application/x-bat", "bx bx-terminal"),
    ("text/x-c++src", "bx bxl-c-plus-plus"),
    ("text/coffeescript", "bx bx-coffee"),
    ("text/css", "bx bxs-file-css"),
    ("text/x-django", "bx bxl-django"),
    ("text/x-dockerfile", "bx bxl-docker"),
    ("application/xml-dtd", "bx bx-code"),
    ("text/x-gfm", "bx bxl-markdown"),
    ("text/x-go", "bx bxl-go-lang"),
    ("text/html", "bx bxl-html5"),
    ("application/x-jsp", "bx bxl-java"),
    ("text/x-java", "bx bxl-java"),
    ("text/javascript", "bx bxl-javascript"),
    ("application/javascript;env=backend", "bx bxl-javascript"),
    ("application/javascript;env=frontend", "bx bxl-javascript"),
    ("application/ld+json", "bx bxs-file-json"),
    ("application/json", "bx bxs-file-json"),
    ("text/x-less", "bx bxl-less"),
    ("text/x-markdown", "bx bxl-markdown"),
    ("text/x-mssql", "bx bx-data"),
    ("text/x-mysql", "bx bx-data"),
    ("text/x-php", "bx bxl-php"),
    ("text/x-pgsql", "bx bxl-postgresql"),
    ("application/x-powershell", "bx bxs-terminal"),
    ("text/x-python", "bx bxl-python"),
    ("text/x-sass", "bx bxl-sass"),
    ("text/x-sh", "bx bx-terminal"),
    ("text/x-sql", "bx bx-data"),
    ("text/x-sqlite;schema=trilium", "bx bx-data"),
    ("text/x-sqlite", "bx bx-data"),
    ("text/x-toml", "bx bx-bracket"),
    ("text/x-trilium-log", "bx bx-list-ul"),
    ("application/typescript", "bx bxl-typescript"),
    ("text/xml", "bx bx-code-alt"),
];

fn note_type_icon(ntype: &str) -> Option<&'static str> {
    Some(match ntype {
        "file" => "bx bx-file", "image" => "bx bx-image", "code" => "bx bx-code", "render" => "bx bx-extension", "search" => "bx bx-file-find",
        "relationMap" | "noteMap" => "bx bxs-network-chart", "book" => "bx bx-book", "mermaid" => "bx bx-selection", "canvas" => "bx bx-pen",
        "webView" => "bx bx-globe-alt", "launcher" => "bx bx-link", "doc" => "bx bxs-file-doc", "contentWidget" => "bx bxs-widget",
        "mindMap" => "bx bx-sitemap", "spreadsheet" => "bx bx-table", "llmChat" => "bx bx-message-square-dots",
        _ => return None,
    })
}

/// Trilium's `getNoteIcon`: the note's own `#iconClass`, else a default from what the note is.
fn note_icon(id: &str, ntype: &str, mime: &str, icon_class: Option<&str>, is_folder: bool, geo_location: bool, geo_shape: Option<&str>) -> Option<String> {
    if let Some(c) = icon_class.filter(|c| !c.trim().is_empty()) { return Some(c.to_string()); }
    if id == "root" { return Some("bx bx-home-alt-2".into()); }
    if id == "_share" { return Some("bx bx-share-alt".into()); }
    let icon: &str = match ntype {
        "text" => {
            if geo_location { "bx bx-pin" }
            else if let Some(shape) = geo_shape.filter(|s| !s.is_empty()) {
                match shape.split(':').next().unwrap_or("") { "polygon" => "bx bx-shape-polygon", "circle" => "bx bx-shape-circle", _ => "bx bx-vector" }
            } else if is_folder { "bx bx-folder" } else { "bx bx-note" }
        }
        "code" => CODE_ICONS.iter().find(|(m, _)| *m == mime).map_or("bx bx-code", |(_, i)| *i),
        "file" => {
            if mime.starts_with("video/") { "bx bx-video" } else if mime.starts_with("audio/") { "bx bx-music" }
            else if mime.starts_with("font/") || mime.starts_with("application/font") || mime.contains("opentype") || mime.contains("truetype") { "bx bx-font" }
            else { match mime {
                "application/pdf" => "bx bxs-file-pdf",
                "application/vnd.oasis.opendocument.text" | "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "bx bxs-file-doc",
                "application/gpx+xml" => "bx bx-trip",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" | "application/vnd.oasis.opendocument.spreadsheet" | "application/vnd.ms-excel" | "text/csv" => "bx bx-spreadsheet",
                _ => "bx bx-file",
            } }
        }
        "image" => if mime == "image/gif" { "bx bxs-file-gif" } else { "bx bx-image" },
        other => note_type_icon(other)?,
    };
    Some(icon.to_string())
}

const THEME_INIT: &str = r#"<script>(function(){var t=null;try{t=localStorage.getItem('theme')}catch(e){}
if(!t)t=window.matchMedia&&window.matchMedia('(prefers-color-scheme: dark)').matches?'dark':'light';
document.documentElement.classList.add('theme-'+t);
try{if(localStorage.getItem('left-pane-collapsed')==='true')document.documentElement.classList.add('left-pane-collapsed');
if(localStorage.getItem('toc-pane-collapsed')==='true')document.documentElement.classList.add('toc-pane-collapsed')}catch(e){}})();</script>"#;

/// Note and attachment ids become file names and URLs: only Trilium's own alphabet is accepted (a sync server controls ids),
/// and `index` is the site's entry page.
fn valid_id(id: &str) -> bool { !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && id != "index" }

/// Percent-encode a relative file path for use in an `href` / `src` (`#`, `%`, spaces, ... in file names).
fn enc_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') { out.push(b as char); } else { out.push_str(&format!("%{b:02X}")); }
    }
    out
}

fn json_for_script(v: &serde_json::Value) -> String {
    v.to_string().replace('<', "\\u003c").replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029")
}

impl Db {
    /// Write the static site to a file (via a `.part` file, like the ZIP export).
    pub fn export_site_file(&self, note_id: &str, path: &Path) -> Result<ExportStats> {
        let part = path.with_file_name(format!(".{}.part", path.file_name().and_then(|n| n.to_str()).unwrap_or("site")));
        let result = (|| -> Result<ExportStats> {
            let f = std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
            let mut w = std::io::BufWriter::new(f);
            let st = self.export_site(note_id, &mut w)?;
            w.flush()?;
            w.get_ref().sync_all()?;
            std::fs::rename(&part, path).with_context(|| format!("moving the export to {}", path.display()))?;
            Ok(st)
        })();
        if result.is_err() { let _ = std::fs::remove_file(&part); }
        result
    }

    pub fn export_site<W: Write + Seek>(&self, note_id: &str, out: W) -> Result<ExportStats> {
        // 1. the notes to publish (each once, at its first placement)
        let mut pages: Vec<Page> = vec![];
        let mut index_of: HashMap<String, usize> = HashMap::new();
        self.collect_pages(note_id, None, 0, &mut pages, &mut index_of)?.context("this note can't be shared (it is protected, excluded from export, or its id is not a plain Trilium id)")?;

        // 2. files: file/image notes and attachments get a stable path under files/
        let mut note_file: HashMap<String, String> = HashMap::new();
        let mut att_file: HashMap<String, String> = HashMap::new();
        let mut file_jobs: Vec<(String, FileSrc)> = vec![];
        for p in &pages {
            if matches!(p.ntype.as_str(), "image" | "file") {
                let name = file_name(&p.title, &p.mime);
                let path = format!("files/{}-{name}", p.id);
                note_file.insert(p.id.clone(), path.clone());
                file_jobs.push((path, FileSrc::Note(p.id.clone())));
            }
            let mut st = self.conn.prepare("SELECT attachmentId,title,mime FROM attachments WHERE ownerId=? AND isDeleted=0 AND isProtected=0 AND role IN ('file','image') ORDER BY position")?;
            let rows: Vec<(String, String, String)> = st.query_map([&p.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            for (aid, title, mime) in rows.into_iter().filter(|(a, ..)| valid_id(a)) {
                let path = format!("files/att-{aid}-{}", file_name(&title, &mime));
                att_file.insert(aid.clone(), path.clone());
                file_jobs.push((path, FileSrc::Attachment(aid)));
            }
        }

        // 3. pages
        let mut zip = zip::ZipWriter::new(out);
        let mut stats = ExportStats::default();
        let mut search: Vec<serde_json::Value> = vec![];
        let ctx = LinkCtx { index_of: &index_of, pages: &pages, note_file: &note_file, att_file: &att_file };
        for (i, p) in pages.iter().enumerate() {
            let (body, plain) = self.page_body(p, &ctx)?;
            let html = self.render_page(i, &ctx, &body)?;
            add_file(&mut zip, &format!("{}.html", p.id), html.as_bytes(), "text/html")?;
            if i == 0 { add_file(&mut zip, "index.html", html.as_bytes(), "text/html")?; }
            search.push(serde_json::json!([p.id, p.title, plain.chars().take(4000).collect::<String>()]));
            stats.notes += 1;
        }
        for (path, src) in file_jobs {
            let (bytes, mime) = match &src {
                FileSrc::Note(id) => (self.note_bytes(id)?, pages[index_of[id]].mime.clone()),
                FileSrc::Attachment(id) => (self.attachment_bytes(id)?, String::new()),
            };
            if let Some(b) = bytes { add_file(&mut zip, &path, &b, &mime)?; if matches!(src, FileSrc::Attachment(_)) { stats.attachments += 1; } }
        }
        let tree: Vec<serde_json::Value> = pages.iter().map(|p| serde_json::json!([p.id, p.title, p.parent.map(|i| pages[i].id.clone()), p.ntype, p.icon])).collect();
        let data = format!("window.TREE={};\nwindow.SEARCH={};\n", json_for_script(&serde_json::Value::Array(tree)), json_for_script(&serde_json::Value::Array(search)));
        add_file(&mut zip, "site-data.js", data.as_bytes(), "application/javascript")?;
        add_file(&mut zip, "site.js", JS.as_bytes(), "application/javascript")?;
        add_file(&mut zip, "share-theme.css", THEME_CSS.as_bytes(), "text/css")?;
        add_file(&mut zip, "site.css", EXTRA_CSS.as_bytes(), "text/css")?;
        add_file(&mut zip, "share-icons.css", ICONS_CSS.as_bytes(), "text/css")?;
        add_file(&mut zip, "boxicons.woff2", ICONS_FONT, "font/woff2")?;
        add_file(&mut zip, "icons-LICENSE.txt", ICONS_NOTICE.as_bytes(), "text/plain")?;
        zip.finish()?;
        Ok(stats)
    }

    fn collect_pages(&self, id: &str, parent: Option<usize>, depth: usize, pages: &mut Vec<Page>, seen: &mut HashMap<String, usize>) -> Result<Option<usize>> {
        if depth > 300 { bail!("the tree is nested more than 300 levels deep; it can't be exported as a site"); }
        if seen.contains_key(id) { return Ok(seen.get(id).copied()); }
        if self.label_value(id, "excludeFromExport")?.is_some() { return Ok(None); }
        let row: Option<(String, String, String, i64)> = self.conn.query_row(
            "SELECT title,type,mime,isProtected FROM notes WHERE noteId=? AND isDeleted=0", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
        let Some((title, ntype, mime, protected)) = row else { return Ok(None) };
        if !valid_id(id) { return Ok(None); }
        if protected != 0 { return Ok(None); }
        let idx = pages.len();
        let attrs = self.attribute_rows(id)?;
        let label = |name: &str| attrs.iter().find(|a| a.kind == "label" && a.name == name).map(|a| a.value.as_str());
        let child_branches: i64 = self.conn.query_row("SELECT COUNT(*) FROM branches WHERE parentNoteId=? AND isDeleted=0", [id], |r| r.get(0))?;
        let icon = note_icon(id, &ntype, &mime, label("iconClass"), child_branches > 0, label("geolocation").is_some_and(|v| !v.is_empty()), label("geoShape"));
        pages.push(Page { id: id.to_string(), title, ntype, mime, parent, children: vec![], icon });
        seen.insert(id.to_string(), idx);
        let mut st = self.conn.prepare("SELECT noteId FROM branches WHERE parentNoteId=? AND isDeleted=0 ORDER BY notePosition")?;
        let kids: Vec<String> = st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for k in kids {
            let already = seen.contains_key(&k);
            if let Some(ci) = self.collect_pages(&k, Some(idx), depth + 1, pages, seen)? {
                if !already { pages[idx].children.push(ci); }
            }
        }
        Ok(Some(idx))
    }

    /// (HTML body, plain text for the search index)
    fn page_body(&self, p: &Page, ctx: &LinkCtx) -> Result<(String, String)> {
        let source = || -> Result<String> { Ok(self.note_content(&p.id)?.unwrap_or_default()) };
        let (html, plain) = match p.ntype.as_str() {
            "text" => {
                let c = source()?;
                let html = sanitize_html(&rewrite_links(&c, ctx));
                let plain = text_content(&parse(&html));
                (html, plain)
            }
            "image" => match ctx.note_file.get(&p.id) {
                Some(f) => (format!(r#"<p><img src="{}" alt="{}"></p>"#, escape_attr(&enc_path(f)), escape_attr(&p.title)), String::new()),
                None => (String::new(), String::new()),
            },
            "file" => match ctx.note_file.get(&p.id) {
                Some(f) => (format!(r#"<p><a href="{}" download>{}</a></p>"#, escape_attr(&enc_path(f)), escape_text(&p.title)), String::new()),
                None => (String::new(), String::new()),
            },
            "code" if matches!(p.mime.as_str(), "text/markdown" | "text/x-markdown" | "text/x-gfm") => {
                let c = source()?;
                let html = sanitize_html(&rewrite_links(&crate::mdhtml::md_to_html(&c, &p.title, false), ctx));
                let plain = text_content(&parse(&html));
                (html, plain)
            }
            "book" | "search" | "contentWidget" | "launcher" | "doc" | "noteMap" | "relationMap" | "aiChat" => (String::new(), String::new()),
            _ if is_string_note(&p.ntype, &p.mime) => {
                let c = source()?;
                (format!("<pre><code>{}</code></pre>", escape_text(&c)), c)
            }
            _ => (String::new(), String::new()),
        };
        Ok((html, plain))
    }

    fn render_page(&self, i: usize, ctx: &LinkCtx, body: &str) -> Result<String> {
        let p = &ctx.pages[i];
        let root = &ctx.pages[0];
        let href = |idx: usize| if idx == 0 { "index.html".to_string() } else { escape_attr(&format!("{}.html", ctx.pages[idx].id)) };
        let has_tree = ctx.pages.len() > 1;
        let page_title = if i == 0 { p.title.clone() } else { format!("{} - {}", p.title, root.title) };

        // headings get anchors, and with more than one of them the page gets a table of contents
        static HEADING: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        let heading = HEADING.get_or_init(|| Regex::new(r"(?s)(<h([1-6])>)(.+?)(</h[1-6]>)").expect("static regex"));
        // ids the theme's own elements already use can't be taken by a heading either
        let mut slugs_used: std::collections::HashSet<String> = ["title", "toc", "menu", "content", "main", "header", "split-pane", "left-pane", "right-pane", "navigation", "site-header", "childlinks", "content-footer", "toc-pane", "sitetheme"].iter().map(|x| x.to_string()).collect();
        let mut entries: Vec<TocEntry> = vec![];
        let content = heading.replace_all(body, |c: &regex::Captures| {
            let name = text_content(&parse(&c[3]));
            let mut slug: String = name.to_lowercase().chars().map(|ch| if ch.is_alphanumeric() { ch } else { '-' }).collect::<String>().split('-').filter(|x| !x.is_empty()).collect::<Vec<_>>().join("-");
            if slug.is_empty() { slug = "section".into(); }
            let base = slug.clone();
            let mut n = 1;
            while slugs_used.contains(&slug) { n += 1; slug = format!("{base}-{n}"); }
            slugs_used.insert(slug.clone());
            entries.push(TocEntry { level: c[2].parse().unwrap_or(2), name, slug: slug.clone(), children: vec![] });
            format!(r##"{}{}<a id="{slug}" class="toc-anchor" name="{slug}" href="#{slug}">#</a>{}"##, &c[1], &c[3], &c[4])
        }).into_owned();
        let toc_html = if entries.len() > 1 {
            let mut toc: Vec<TocEntry> = vec![];
            for e in entries { toc_insert(&mut toc, e); }
            format!(r#"<div id="toc-pane"><h3>On this page</h3><ul id="toc">{}</ul></div>"#, toc.iter().map(toc_item).collect::<String>())
        } else { String::new() };
        let toc_button = if toc_html.is_empty() { r#"<div class="header-button-placeholder"></div>"#.to_string() } else {
            r#"<button aria-label="Toggle table of contents" id="toc-pane-toggle-button" class="header-button"><svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="currentColor"><path d="M3 9h14V7H3v2zm0 4h14v-2H3v2zm0 4h14v-2H3v2zm16 0h2v-2h-2v2zm0-10V7h2v2h-2zm0 6h2v-2h-2v2z"></path></svg></button>"#.to_string()
        };

        let is_empty = content.trim().is_empty() && !content.contains("<img");
        let child_links = if p.children.is_empty() { String::new() } else {
            format!(r#"<nav id="childLinks" class="{}">{}<ul>{}</ul></nav>"#, if is_empty { "grid" } else { "list" }, if is_empty { "" } else { "<span>Subpages</span>" },
                p.children.iter().map(|c| format!(r#"<li><a class="type-{}" href="{}">{}</a></li>"#, escape_attr(&ctx.pages[*c].ntype), href(*c), escape_text(&ctx.pages[*c].title))).collect::<String>())
        };
        let mut st = self.conn.prepare("SELECT attachmentId,title FROM attachments WHERE ownerId=? AND isDeleted=0 AND isProtected=0 AND role IN ('file','image') ORDER BY position")?;
        let atts: Vec<(String, String)> = st.query_map([&p.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let atts_html = {
            let items: String = atts.iter().filter_map(|(aid, t)| ctx.att_file.get(aid).map(|f| format!(r#"<li><a href="{}" download>{}</a></li>"#, escape_attr(&enc_path(f)), escape_text(t)))).collect();
            if items.is_empty() { String::new() } else { format!(r#"<div class="attachments"><strong>Attachments</strong><ul>{items}</ul></div>"#) }
        };
        // previous / next follow the tree in reading order (pages are collected depth-first)
        let nav = if has_tree {
            let prev = if i > 0 { format!(r#"<a class="previous" href="{}">{}</a>"#, href(i - 1), escape_text(&ctx.pages[i - 1].title)) } else { String::new() };
            let next = if i + 1 < ctx.pages.len() { format!(r#"<a class="next" href="{}">{}</a>"#, href(i + 1), escape_text(&ctx.pages[i + 1].title)) } else { String::new() };
            format!(r#"<div class="navigation">{prev}{next}</div>"#)
        } else { String::new() };
        let ck = if p.ntype == "text" || (p.ntype == "code" && matches!(p.mime.as_str(), "text/markdown" | "text/x-markdown" | "text/x-gfm")) { " ck-content" } else { "" };
        let tree_ui = if has_tree {
            r#"<div class="search-item"><svg class="search-icon" xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="currentColor"><path d="M10 18a7.952 7.952 0 0 0 4.897-1.688l4.396 4.396 1.414-1.414-4.396-4.396A7.952 7.952 0 0 0 18 10c0-4.411-3.589-8-8-8s-8 3.589-8 8 3.589 8 8 8zm0-14c3.309 0 6 2.691 6 6s-2.691 6-6 6-6-2.691-6-6 2.691-6 6-6z"></path></svg><input type="text" class="search-input" placeholder="Search"></div>"#
        } else { "" };
        let menu = if has_tree { r#"<nav id="menu"></nav>"# } else { "" };
        Ok(format!(
            r##"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{title}</title><link href="share-theme.css" rel="stylesheet"><link href="share-icons.css" rel="stylesheet"><link href="site.css" rel="stylesheet">{THEME_INIT}</head>
<body data-id="{id}" data-note-id="{id}" class="type-{ty}">
<div id="header"><button aria-label="Toggle navigation" id="left-pane-toggle-button" class="header-button"><svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="currentColor"><path d="M4 6h16v2H4zm0 5h16v2H4zm0 5h16v2H4z"></path></svg></button><a href="index.html" id="header-logo">{root_title}</a>{toc_button}</div>
<div id="split-pane"><div id="left-pane"><div id="navigation"><div id="site-header"><div class="theme-selection"><span id="sitetheme">Site theme</span><label class="switch"><input type="checkbox" aria-labelledby="sitetheme"><span class="slider"></span></label></div>{tree_ui}</div>{menu}</div></div>
<div id="right-pane"><div id="main"><div id="content" class="type-{ty}{ck}{empty}"><h1 id="title">{t}</h1>
{content}
</div>{child_links}{atts_html}<footer id="content-footer">{nav}</footer></div>{toc_html}</div></div>
<script src="site-data.js"></script><script src="site.js"></script></body></html>
"##,
            title = escape_text(&page_title), id = escape_attr(&p.id), ty = escape_attr(&p.ntype), root_title = escape_text(&root.title), t = escape_text(&p.title),
            empty = if is_empty { " no-content" } else { "" }))
    }
}

struct TocEntry { level: usize, name: String, slug: String, children: Vec<TocEntry> }

/// Trilium's nesting: a heading goes into the list whose level matches, descending into the last entry's children.
fn toc_insert(toc: &mut Vec<TocEntry>, e: TocEntry) {
    fn level_list(arr: &mut Vec<TocEntry>, lvl: usize) -> &mut Vec<TocEntry> {
        if arr[0].level == lvl { return arr; }
        let last = arr.len() - 1;
        if arr[last].children.is_empty() { &mut arr[last].children } else { level_list(&mut arr[last].children, lvl) }
    }
    if toc.is_empty() { toc.push(e); return; }
    let lvl = e.level;
    level_list(toc, lvl).push(e);
}

fn toc_item(e: &TocEntry) -> String {
    let kids = if e.children.is_empty() { String::new() } else { format!("<ul>{}</ul>", e.children.iter().map(toc_item).collect::<String>()) };
    format!(r##"<li><a href="#{}"><span>{}</span></a>{kids}</li>"##, escape_attr(&e.slug), escape_text(&e.name))
}

enum FileSrc { Note(String), Attachment(String) }

struct LinkCtx<'a> {
    index_of: &'a HashMap<String, usize>,
    pages: &'a [Page],
    note_file: &'a HashMap<String, String>,
    att_file: &'a HashMap<String, String>,
}

fn file_name(title: &str, mime: &str) -> String {
    let mut n = sanitize_filename(title);
    if n.is_empty() { n = "file".into(); }
    if extname(&n).is_empty() {
        if let Some(e) = mime_guess::get_mime_extensions_str(mime).and_then(|e| e.first()) { n = format!("{n}.{e}"); }
    }
    n
}

/// Point Trilium-internal links and media at the pages and files of the site.
fn rewrite_links(html: &str, ctx: &LinkCtx) -> String {
    use std::sync::OnceLock;
    static ATT_LINK: OnceLock<Regex> = OnceLock::new();
    static NOTE_LINK: OnceLock<Regex> = OnceLock::new();
    static IMG: OnceLock<Regex> = OnceLock::new();
    static ATT: OnceLock<Regex> = OnceLock::new();
    let att_link = ATT_LINK.get_or_init(|| Regex::new(r##"href="#root/(?:[^"/?]+/)*[^"/?]+\?viewMode=attachments&(?:amp;)?attachmentId=([A-Za-z0-9_]+)""##).unwrap());
    let note_link = NOTE_LINK.get_or_init(|| Regex::new(r##"href="#(root/(?:[^"/?#]+/)*)?([A-Za-z0-9_]+)(?:\?[^"]*)?""##).unwrap());
    let img = IMG.get_or_init(|| Regex::new(r#"(src|href)="(?:\.\./)*/?api/images/([A-Za-z0-9_]+)/[^"]*""#).unwrap());
    let att = ATT.get_or_init(|| Regex::new(r#"(src|href)="(?:\.\./)*/?api/attachments/([A-Za-z0-9_]+)/(?:download|image/[^"]*)""#).unwrap());
    let out = att_link.replace_all(html, |c: &regex::Captures| match ctx.att_file.get(&c[1]) {
        Some(f) => format!(r#"href="{}""#, enc_path(f)),
        None => r##"href="#""##.to_string(),
    });
    let out = note_link.replace_all(&out, |c: &regex::Captures| match (ctx.index_of.contains_key(&c[2]), c.get(1).is_some()) {
        (true, _) => format!(r#"href="{}.html""#, &c[2]),
        (false, true) => r##"href="#""##.to_string(),
        // a plain in-page anchor (#top, #fn1) stays as it is
        (false, false) => c[0].to_string(),
    });
    let out = img.replace_all(&out, |c: &regex::Captures| match ctx.note_file.get(&c[2]) {
        Some(f) => format!(r#"{}="{}""#, &c[1], enc_path(f)),
        None => format!(r#"{}="""#, &c[1]),
    });
    att.replace_all(&out, |c: &regex::Captures| match ctx.att_file.get(&c[2]) {
        Some(f) => format!(r#"{}="{}""#, &c[1], enc_path(f)),
        None => format!(r#"{}="""#, &c[1]),
    }).into_owned()
}

#[allow(dead_code)]
fn _keep(_: &str) -> String { decode_entities("") }

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("s.db")).unwrap();
        db.set_state("instanceId", "t").unwrap();
        db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified)
            VALUES('root','root',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", []).unwrap();
        db.record_entity("notes", "noteId", "root", "utcDateModified").unwrap();
        (dir, db)
    }

    #[test]
    fn site_has_pages_links_files_and_no_active_content() {
        let (_d, db) = fixture();
        let home = db.create_note("root", "Home <&>", "text", "text/html", "").unwrap();
        let other = db.create_note(&home, "Other", "text", "text/html", "<p>second page</p>").unwrap();
        let secret = db.create_note(&home, "Secret", "text", "text/html", "hidden").unwrap();
        db.add_attribute(&secret, "label", "excludeFromExport", "", false).unwrap();
        let img = db.create_file_note(&home, "pic.png", "image/png", b"\x89PNG-bytes").unwrap();
        let code = db.create_note(&home, "Code", "code", "text/x-python", "print('<hi>')").unwrap();
        let att = db.add_attachment(&other, "doc.txt", "text/plain", "file", b"attached").unwrap();
        let body = format!(r##"<p onclick="evil()">see <a class="reference-link" href="#root/{home}/{other}">Other</a> and <a href="#root/{secret}">gone</a>
            <img src="api/images/{img}/pic.png"> <a href="api/attachments/{att}/download">dl</a></p><script>alert(1)</script>"##);
        db.set_note_content(&home, &body).unwrap();

        let mut buf = Cursor::new(vec![]);
        let st = db.export_site(&home, &mut buf).unwrap();
        assert_eq!(st.notes, 4, "home, other, image, code; the excluded note is left out");
        let mut z = zip::ZipArchive::new(Cursor::new(buf.into_inner())).unwrap();
        let mut read = |n: &str| { let mut s = vec![]; z.by_name(n).unwrap_or_else(|_| panic!("missing {n}")).read_to_end(&mut s).unwrap(); s };
        let index = String::from_utf8(read("index.html")).unwrap();
        assert_eq!(index, String::from_utf8(read(&format!("{home}.html"))).unwrap());
        assert!(index.contains("<title>Home &lt;&amp;&gt;</title>"), "{index}");
        assert!(index.contains(r#"<div id="content" class="type-text ck-content""#) && index.contains(r#"id="header-logo""#) && index.contains("share-theme.css"), "Trilium's page structure");
        assert!(index.contains(&format!(r#"href="{other}.html">Other</a>"#)), "{index}");
        assert!(index.contains(r##"href="#">gone</a>"##), "links to unexported notes are neutralised");
        assert!(index.contains(&format!(r#"src="files/{img}-pic.png""#)) && index.contains(&format!(r#"<li><a class="type-text" href="{other}.html">Other</a></li>"#)));
        for bad in ["onclick", "<script>alert", "evil("] { assert!(!index.contains(bad), "{bad}"); }
        assert_eq!(read(&format!("files/{img}-pic.png")), b"\x89PNG-bytes");
        let other_page = String::from_utf8(read(&format!("{other}.html"))).unwrap();
        assert!(other_page.contains("second page") && other_page.contains(&format!("files/att-{att}-doc.txt")), "{other_page}");
        assert_eq!(read(&format!("files/att-{att}-doc.txt")), b"attached");
        assert!(String::from_utf8(read(&format!("{code}.html"))).unwrap().contains("print('&lt;hi&gt;')"));
        let data = String::from_utf8(read("site-data.js")).unwrap();
        assert!(data.starts_with("window.TREE=[[") && data.contains("second page") && !data.contains("Secret"), "{data}");
        assert!(!data.contains("</script"), "data can't break out of its script tag");
        assert!(data.contains(r#""bx bx-note""#) && data.contains(r#""bx bx-image""#) && data.contains(r#""bx bxl-python""#), "Trilium's default note icons: {data}");
        assert!(read("share-icons.css").len() > 20_000 && read("boxicons.woff2").len() > 50_000 && String::from_utf8(read("icons-LICENSE.txt")).unwrap().contains("Attribution"));
        let c = LinkCtx { index_of: &HashMap::new(), pages: &[], note_file: &HashMap::new(), att_file: &HashMap::new() };
        assert_eq!(rewrite_links(r##"<a href="#top">t</a><a href="#fn1">f</a><a href="#root/abc123">x</a>"##, &c), r##"<a href="#top">t</a><a href="#fn1">f</a><a href="#">x</a>"##);
        assert!(read("site.js").len() > 100 && read("share-theme.css").len() > 10_000 && read("site.css").len() > 50);
        // reading-order navigation, and a table of contents when a page has several headings
        let other_page = String::from_utf8(read(&format!("{other}.html"))).unwrap();
        assert!(other_page.contains(r#"class="previous" href="index.html">Home"#) && other_page.contains(r#"class="next""#), "{other_page}");
        let h = db.create_note(&home, "Headed", "text", "text/html", "<h2>Intro</h2><p>a</p><h3>Sub &amp; part</h3><h2>Intro</h2>").unwrap();
        let mut buf = Cursor::new(vec![]);
        db.export_site(&home, &mut buf).unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(buf.into_inner())).unwrap();
        let mut page = String::new();
        z.by_name(&format!("{h}.html")).unwrap().read_to_string(&mut page).unwrap();
        assert!(page.contains(r##"<h2>Intro<a id="intro" class="toc-anchor" name="intro" href="#intro">#</a></h2>"##) && page.contains(r#"id="intro-2""#), "{page}");
        assert!(page.contains(r##"<ul id="toc"><li><a href="#intro"><span>Intro</span></a><ul><li><a href="#sub-part"><span>Sub &amp; part</span></a></li></ul></li><li><a href="#intro-2">"##), "{page}");
    }

    #[test]
    fn note_icons_follow_trilium() {
        assert_eq!(note_icon("x", "text", "text/html", None, false, false, None).unwrap(), "bx bx-note");
        assert_eq!(note_icon("x", "text", "text/html", None, true, false, None).unwrap(), "bx bx-folder");
        assert_eq!(note_icon("x", "text", "text/html", Some("bx bx-star"), true, false, None).unwrap(), "bx bx-star");
        assert_eq!(note_icon("x", "text", "text/html", None, true, true, None).unwrap(), "bx bx-pin", "a place beats the folder icon");
        assert_eq!(note_icon("x", "code", "text/x-python", None, false, false, None).unwrap(), "bx bxl-python");
        assert_eq!(note_icon("x", "code", "text/x-unknown", None, false, false, None).unwrap(), "bx bx-code");
        assert_eq!(note_icon("x", "file", "application/pdf", None, false, false, None).unwrap(), "bx bxs-file-pdf");
        assert_eq!(note_icon("x", "file", "video/mp4", None, false, false, None).unwrap(), "bx bx-video");
        assert_eq!(note_icon("x", "image", "image/gif", None, false, false, None).unwrap(), "bx bxs-file-gif");
        assert_eq!(note_icon("root", "text", "text/html", None, true, false, None).unwrap(), "bx bx-home-alt-2");
        assert_eq!(note_icon("x", "book", "", None, false, false, None).unwrap(), "bx bx-book");
        assert!(note_icon("x", "nonsense", "", None, false, false, None).is_none());
        // every icon we hand out has a glyph in the shipped table
        for c in ["bx-note", "bx-folder", "bxl-python", "bxs-file-pdf", "bx-home-alt-2", "bx-pin", "bx-spreadsheet", "bxs-widget"] { assert!(ICONS_CSS.contains(&format!(".{c}::before")), "{c}"); }
    }

    #[test]
    fn protected_or_excluded_roots_are_refused() {
        let (_d, db) = fixture();
        let n = db.create_note("root", "x", "text", "text/html", "").unwrap();
        db.add_attribute(&n, "label", "excludeFromExport", "", false).unwrap();
        assert!(db.export_site(&n, Cursor::new(vec![])).is_err());
        assert!(db.export_site("nope", Cursor::new(vec![])).is_err());
    }

    #[test]
    fn hostile_ids_and_awkward_file_names_stay_inside_the_site() {
        let (_d, db) = fixture();
        let home = db.create_note("root", "Home", "text", "text/html", "<h2>title</h2><h2>toc</h2><h2>title</h2>").unwrap();
        let f = db.create_file_note(&home, "a#b 100%.txt", "text/plain", b"hi").unwrap();
        db.conn.execute("UPDATE notes SET title=title WHERE 0", []).unwrap();
        // notes whose ids a sync server chose
        for bad in ["../../evil", "index", "a\"b", "javascript:alert(1)//"] {
            db.conn.execute("INSERT INTO notes(noteId,title,isProtected,type,mime,blobId,isDeleted,dateCreated,dateModified,utcDateCreated,utcDateModified) VALUES(?,'x',0,'text','text/html',NULL,0,'x','x','2020-01-01 00:00:00.000Z','2020-01-01 00:00:00.000Z')", [bad]).unwrap();
            db.conn.execute("INSERT INTO branches(branchId,noteId,parentNoteId,notePosition,isExpanded,isDeleted,utcDateModified) VALUES(?,?,?,10,0,0,'2020-01-01 00:00:00.000Z')", [format!("{home}_{bad}"), bad.to_string(), home.clone()]).unwrap();
        }
        let mut buf = Cursor::new(vec![]);
        db.export_site(&home, &mut buf).unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(buf.into_inner())).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name_raw().iter().map(|&b| b as char).collect::<String>()).collect();
        assert!(names.iter().all(|n| !n.contains("..") && !n.starts_with('/')), "{names:?}");
        let mut page = String::new();
        z.by_name("index.html").unwrap().read_to_string(&mut page).unwrap();
        assert!(!page.contains("javascript:") && !page.contains(r#"href="a"b"#), "{page}");
        let mut file_page = String::new();
        z.by_name(&format!("{f}.html")).unwrap().read_to_string(&mut file_page).unwrap();
        assert!(file_page.contains("-a%23b%20100%25.txt\"") && !file_page.contains("files/") || file_page.contains("a%23b%20100%25.txt"), "file links are percent-encoded: {file_page}");
        assert!(page.contains(r#"id="title-2""#) && page.contains(r#"id="toc-2""#) && page.contains(r#"id="title-3""#), "{page}");
        let _ = f;
        let mut z2 = zip::ZipWriter::new(Cursor::new(vec![]));
        assert!(add_file(&mut z2, "../x.html", b"", "text/html").is_err() && add_file(&mut z2, "/abs", b"", "text/html").is_err() && add_file(&mut z2, "ok/x.html", b"", "text/html").is_ok());
    }
}
