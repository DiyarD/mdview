// mdview — extremely efficient markdown viewer.
// Tiny idle cost by design: NO background webview. A ~1MB tray helper owns a
// real global hotkey and launches us on demand; single WebView2 + Rust
// pulldown-cmark rendering when open.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::borrow::Cow;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::json;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::windows::WindowExtWindows;
use tao::window::{Fullscreen, WindowBuilder};
use wry::{DragDropEvent, NewWindowResponse, PageLoadEvent, WebViewBuilder};

mod appicon;
mod db;

const SINGLE_ADDR: &str = "127.0.0.1:17842";
const MAX_MD_BYTES: usize = 4 * 1024 * 1024; // 4MB cap per paste keeps memory flat
const MAX_FETCH_BYTES: usize = 3 * 1024 * 1024;
const CF_HDROP_FMT: u32 = 15; // clipboard format holding dropped file paths
const MAX_DOC_BATCH: usize = 8; // one drop/pick opens at most this many docs

#[derive(Debug)]
enum UserEvent {
    IpcEval(String),
    NewTabRemote,
    OpenTabRemote { name: String, text: String, base: String, source: String },
    ToggleFullscreen(String),
    SetTitle(String),
    ShowWindow,
    PickFile { id: String },
}

/// Open a URL/path in the system default handler (browser / media player).
/// CREATE_NO_WINDOW: cmd.exe is a console app — without this every click
/// flashes a terminal.
fn open_external(target: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", target])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Theme-aware startup background so first paint isn't a white flash.
/// Prefers the stored pref; on first run asks Windows (std-only reg query).
fn startup_bg() -> (u8, u8, u8, u8) {
    const DARK: (u8, u8, u8, u8) = (26, 21, 18, 255); // warm charcoal
    const LIGHT: (u8, u8, u8, u8) = (250, 247, 241, 255); // paper
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&load_prefs_json()) {
        match v.get("theme").and_then(|t| t.as_str()) {
            Some("dark") => return DARK,
            Some("light") => return LIGHT,
            _ => {}
        }
    }
    #[cfg(target_os = "windows")]
    {
        // AppsUseLightTheme=0 => dark mode. ~20ms one-shot at startup.
        // CREATE_NO_WINDOW: reg.exe is a console app, never flash it.
        use std::os::windows::process::CommandExt;
        if let Ok(out) = std::process::Command::new("reg")
            .args([
                "query",
                "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
                "/v",
                "AppsUseLightTheme",
            ])
            .creation_flags(0x08000000)
            .output()
        {
            let txt = String::from_utf8_lossy(&out.stdout);
            if txt.contains("0x0") {
                return DARK;
            }
        }
    }
    LIGHT
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--install-hotkey") {
        std::process::exit(install_hotkey());
    }
    if args.iter().any(|a| a == "--uninstall-hotkey") {
        std::process::exit(uninstall_hotkey());
    }

    // Possible file/URL argument (Explorer "Open with" or CLI).
    let open_arg: Option<String> = args.iter().find(|a| !a.starts_with("--")).cloned();

    // ---- single instance: second launch just signals the first, then exits ----
    let primary_listener: TcpListener = match TcpListener::bind(SINGLE_ADDR) {
        Ok(l) => l,
        Err(_) => {
            signal_primary(&open_arg);
            return;
        }
    };

    let initial = open_arg
        .as_deref()
        .map(resolve_open_arg)
        .unwrap_or(InitialDoc { name: String::new(), text: String::new(), base: String::new(), source: String::new() });

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    spawn_accept_loop(primary_listener, proxy.clone());

    // Hidden until first paint: kills the white-flash window (see ShowWindow).
    let bg = startup_bg();
    // Warm-ink M mark (paper tile would vanish on light taskbars; the tray
    // keeps the blue variant so the two are distinguishable).
    let win_icon =
        tao::window::Icon::from_rgba(appicon::m_rgba((44, 38, 32), (250, 247, 241), 64), 64, 64).ok();
    let window = WindowBuilder::new()
        .with_title("mdview — paste anything")
        .with_inner_size(LogicalSize::new(1100.0, 750.0))
        .with_min_inner_size(LogicalSize::new(480.0, 360.0))
        .with_visible(false)
        .with_background_color(bg)
        .with_window_icon(win_icon)
        .build(&event_loop)
        .expect("create window");

    // Fallback: never stay invisible if the page-load event misfires.
    {
        let p = proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2500));
            let _ = p.send_event(UserEvent::ShowWindow);
        });
    }

    let html = build_html(&initial);
    let proxy_ipc = proxy.clone();
    let proxy_page = proxy.clone();
    let proxy_nav = proxy.clone();
    let proxy_nw = proxy.clone();
    let proxy_drop = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_html(html)
        .with_background_color(bg)
        .with_clipboard(true)
        .with_devtools(cfg!(debug_assertions))
        .with_navigation_handler(move |url| {
            // Our own assets (KaTeX) and local files must never escape to the
            // browser: a subresource load never reaches here, but a stray
            // top-level navigation to one would. Check before the http branch.
            if url.contains("/asset/") {
                return false;
            }
            // Internal "link might be an image" handler URL: never navigate.
            if url.contains("/fetch?url=") {
                return false;
            }
            // Local asset/doc links (http://mdview.localhost/local/... —
            // wry's workaround form, plus legacy mdview://). MUST be checked
            // before the http branch below, or they'd open in the browser.
            if url.contains("/local/") {
                // JS handles doc/media clicks via IPC too; never let this
                // replace the viewer content.
                if let Some(path) = decode_local_media_url(&url) {
                    if is_doc_path(&path) {
                        if let Ok(text) = read_file_limited(&path, MAX_MD_BYTES) {
                            let _ = proxy_nav.send_event(UserEvent::OpenTabRemote {
                                name: short_name(&path),
                                text,
                                base: parent_dir_of(&path),
                                source: path.clone(),
                            });
                        }
                    } else {
                        open_external(&path);
                    }
                }
                return false;
            }
            // Keep the viewer a viewer: external links open in the real browser.
            if is_http_url(&url) {
                open_external(&url);
                return false;
            }
            true
        })
        .with_new_window_req_handler(move |url, _| {
            // <a target="_blank"> clicks land here, NOT in the navigation
            // handler — this was why links silently did nothing. Local refs
            // get the same treatment as above (never the external browser).
            if url.contains("/asset/") {
                return NewWindowResponse::Deny;
            }
            if url.contains("/local/") {
                if let Some(path) = decode_local_media_url(&url) {
                    if is_doc_path(&path) {
                        if let Ok(text) = read_file_limited(&path, MAX_MD_BYTES) {
                            let _ = proxy_nw.send_event(UserEvent::OpenTabRemote {
                                name: short_name(&path),
                                text,
                                base: parent_dir_of(&path),
                                source: path.clone(),
                            });
                        }
                    } else {
                        open_external(&path);
                    }
                }
                return NewWindowResponse::Deny;
            }
            if is_http_url(&url) {
                open_external(&url);
            }
            NewWindowResponse::Deny
        })
        .with_on_page_load_handler(move |event, _| {
            if matches!(event, PageLoadEvent::Finished) {
                let _ = proxy_page.send_event(UserEvent::ShowWindow);
            }
        })
        .with_asynchronous_custom_protocol("mdview".into(), |_id, request, responder| {
            let uri = request.uri().to_string();
            std::thread::spawn(move || {
                let r = serve_app_asset(&uri)
                    .or_else(|| serve_remote_media(&uri))
                    .unwrap_or_else(|| serve_local_media(&uri));
                responder.respond(r);
            });
        })
        .with_ipc_handler(move |req| {
            handle_ipc(req.body(), &proxy_ipc);
        })
        .with_drag_drop_handler(move |event| {
            // Native OLE drop: the one place Windows hands us the REAL
            // paths of dragged files (WebView2's HTML5 DnD hides them by
            // design). Rust opens each doc with its directory, so relative
            // images serve with zero clicks — into the unloaded tab.
            if let DragDropEvent::Drop { paths, .. } = event {
                for d in docs_from_paths(&paths) {
                    let _ = proxy_drop.send_event(UserEvent::OpenTabRemote {
                        name: d.name,
                        text: d.text,
                        base: d.base,
                        source: d.source,
                    });
                }
            }
            true
        })
        .build(&window)
        .expect("build webview");

    let mut is_fullscreen = false;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
                WindowEvent::DroppedFile(path) => {
                    // OS-level file drop (JS drop handler covers the WebView area too).
                    if let Some(doc) = read_path_as_doc(&path) {
                        let _ = proxy.send_event(UserEvent::OpenTabRemote {
                            name: doc.name,
                            text: doc.text,
                            base: doc.base,
                            source: doc.source,
                        });
                    }
                }
                _ => {}
            },
            Event::UserEvent(ue) => match ue {
                UserEvent::IpcEval(js) => {
                    let _ = webview.evaluate_script(&js);
                }
                UserEvent::NewTabRemote => {
                    window.set_focus();
                    window.set_visible(true);
                    let _ = webview.evaluate_script(
                        "window.__mdview_new_tab_remote && window.__mdview_new_tab_remote();",
                    );
                }
                UserEvent::OpenTabRemote { name, text, base, source } => {
                    window.set_focus();
                    window.set_visible(true);
                    let payload = serde_json::to_string(&json!({ "name": name, "text": text, "base": base, "source": source }))
                        .unwrap_or_else(|_| "{\"name\":\"pasted\",\"text\":\"\",\"base\":\"\",\"source\":\"\"}".into());
                    let js = format!(
                        "window.__mdview_open_payload && window.__mdview_open_payload({});",
                        payload
                    );
                    let _ = webview.evaluate_script(&js);
                }
                UserEvent::SetTitle(title) => {
                    // The window chrome follows the active document, so renaming
                    // a tab is visible without opening the tab.
                    window.set_title(&format!("mdview — {}", title));
                }
                UserEvent::PickFile { id } => {
                    // Modal file dialog runs on the UI thread (owner window + z-order).
                    let id_json = serde_json::to_string(&id).unwrap_or("\"\"".into());
                    let js = match pick_file_dialog(window.hwnd()) {
                        Ok(paths) => {
                            let payload = serde_json::to_string(&docs_from_paths(&paths))
                                .unwrap_or_else(|_| "[]".into());
                            format!(
                                "window.__mdview_ipc_result && window.__mdview_ipc_result({}, true, {});",
                                id_json, payload
                            )
                        }
                        Err(msg) => format!(
                            "window.__mdview_ipc_result && window.__mdview_ipc_result({}, false, {});",
                            id_json,
                            serde_json::to_string(&msg).unwrap_or("\"error\"".into())
                        ),
                    };
                    let _ = webview.evaluate_script(&js);
                }
                UserEvent::ToggleFullscreen(id) => {
                    is_fullscreen = !is_fullscreen;
                    window.set_fullscreen(if is_fullscreen {
                        Some(Fullscreen::Borderless(None))
                    } else {
                        None
                    });
                    let js = format!(
                        "window.__mdview_ipc_result && window.__mdview_ipc_result({}, true, {});",
                        serde_json::to_string(&id).unwrap_or("\"\"".into()),
                        if is_fullscreen { "true" } else { "false" }
                    );
                    let _ = webview.evaluate_script(&js);
                }
                UserEvent::ShowWindow => {
                    window.set_visible(true);
                    window.set_focus();
                }
            },
            _ => {}
        }
    });
}

/* ================= markdown rendering (Rust, streaming, low-alloc) ================= */

/// Every markdown option the renderer understands — block-span extraction must
/// see exactly the same document the HTML renderer sees.
fn md_options() -> pulldown_cmark::Options {
    use pulldown_cmark::Options;
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    opts.insert(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS);
    opts.insert(Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS);
    // Math is parsed here (so GFM emphasis can't mangle subscripts like
    // $s_{it}$) but NOT typeset here: it lands as
    // <span class="math math-inline|math-display">latex</span>, which the
    // page hands to KaTeX directly. (Rendering the MathML pulldown-cmark
    // would otherwise emit needs a full MathML typesetter, and stripping the
    // $ delimiters breaks KaTeX's delimiter-driven auto-render.)
    opts.insert(Options::ENABLE_MATH);
    opts.insert(Options::ENABLE_GFM);
    opts
}

/// The kind of one top-level markdown block — used to tag rendered blocks for
/// the editor (and for sanity-checking the DOM). `None` = not a top-level
/// block start.
fn top_block_kind(tag: &pulldown_cmark::Tag) -> Option<&'static str> {
    use pulldown_cmark::Tag;
    Some(match tag {
        Tag::Heading { .. } => "heading",
        Tag::Paragraph => "paragraph",
        Tag::CodeBlock(..) => "code",
        Tag::BlockQuote(..) => "quote",
        Tag::List(..) => "list",
        Tag::Table(..) => "table",
        Tag::FootnoteDefinition(..) => "footnote",
        Tag::HtmlBlock => "html",
        Tag::MetadataBlock(..) => "frontmatter",
        Tag::DefinitionList => "deflist",
        _ => return None,
    })
}

/// One top-level markdown block, in line coordinates. Byte offsets are
/// deliberately not exposed: the page is UTF-16 and we refuse to let JS do
/// byte math. `start_line` inclusive, `end_line` exclusive.
#[derive(serde::Serialize, Debug, Clone, PartialEq)]
pub(crate) struct BlockSpan {
    pub index: usize,
    pub kind: &'static str,
    pub start_line: usize,
    pub end_line: usize,
}

/// Line starts (byte offset of each line's first byte) and the line of a byte.
fn line_table(md: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in md.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn line_of(table: &[usize], byte: usize) -> usize {
    match table.binary_search(&byte) {
        Ok(n) => n,
        Err(n) => n - 1, // table[0] == 0 <= byte, so n >= 1
    }
}

/// Collect top-level block spans of the (already truncated) document. Runs
/// the exact same parser configuration as the renderer.
pub(crate) fn block_spans(md: &str) -> Vec<BlockSpan> {
    use pulldown_cmark::{Event, Parser};
    let opts = md_options();
    let table = line_table(md);
    let mut spans = Vec::new();
    let mut depth: i32 = 0;
    let mut open: Vec<Option<&'static str>> = Vec::new();
    let mut index = 0usize;
    // pulldown assigns the trailing blank line after a list to the list item,
    // so trim trailing blank-only lines: a span must end on real content.
    let lines: Vec<&str> = md.split('\n').collect();
    let trim_end = |start_line: usize, mut end_line: usize| -> usize {
        while end_line > start_line && lines[end_line - 1].trim().is_empty() {
            end_line -= 1;
        }
        end_line
    };
    for (ev, range) in Parser::new_ext(md, opts).into_offset_iter() {
        match ev {
            Event::Start(ref tag) => {
                if depth == 0 {
                    let kind = top_block_kind(tag);
                    if kind.is_some() { open.push(Some(kind_of(tag))); } else { open.push(None); }
                } else {
                    open.push(None);
                }
                depth += 1;
            }
            Event::End(_) => {
                depth -= 1;
                if let Some(Some(kind)) = open.pop() {
                    if depth == 0 {
                        let start_line = line_of(&table, range.start);
                        let end_line = trim_end(start_line, line_of(&table, range.end.saturating_sub(1).max(range.start)) + 1);
                        spans.push(BlockSpan { index, kind, start_line, end_line });
                        index += 1;
                    }
                }
            }
            Event::Rule | Event::DisplayMath(_) => {
                if depth == 0 {
                    let kind = if matches!(ev, Event::Rule) { "rule" } else { "math" };
                    let start_line = line_of(&table, range.start);
                    let end_line = trim_end(start_line, line_of(&table, range.end.saturating_sub(1).max(range.start)) + 1);
                    spans.push(BlockSpan { index, kind, start_line, end_line });
                    index += 1;
                }
            }
            _ => {}
        }
    }
    spans
}

fn kind_of(tag: &pulldown_cmark::Tag) -> &'static str {
    top_block_kind(tag).unwrap_or("unknown")
}

fn render_markdown_to_html(md: &str, base: Option<&str>) -> String {
    use pulldown_cmark::{html, Parser};
    let md = truncate_str(md, MAX_MD_BYTES);
    let opts = md_options();
    // Render block-by-block instead of one push_html pass: each top-level
    // block becomes <div class="md-block" data-blk="N">…</div>. The page maps
    // rendered elements back to source lines via data-blk (display:contents
    // in CSS keeps the wrapper boxless). Per-block rendering is exact: the
    // event stream, not HTML string surgery, decides block boundaries.
    let mut out = String::with_capacity(md.len().min(1 << 20));
    let mut buf: Vec<pulldown_cmark::Event<'_>> = Vec::new();
    let mut depth: i32 = 0;
    let mut top_open = 0usize;
    let mut blk_index = 0usize;
    let flush = |out: &mut String, buf: &mut Vec<pulldown_cmark::Event<'_>>, blk_index: &mut usize, wrapped: bool| {
        if buf.is_empty() {
            return;
        }
        if wrapped {
            out.push_str("<div class=\"md-block\" data-blk=\"");
            out.push_str(&blk_index.to_string());
            out.push_str("\">");
            *blk_index += 1;
        }
        html::push_html(out, buf.drain(..));
        if wrapped {
            out.push_str("</div>\n");
        }
    };
    for (ev, _range) in Parser::new_ext(&md, opts).into_offset_iter() {
        let ev = ev.into_static();
        let opens_block = matches!(&ev, pulldown_cmark::Event::Start(tag) if top_block_kind(tag).is_some());
        if depth == 0 && opens_block && !buf.is_empty() {
            // Streams from our option set never do this; flush the stray
            // buffer rather than losing its bytes.
            flush(&mut out, &mut buf, &mut blk_index, false);
        }
        match &ev {
            pulldown_cmark::Event::Start(_) => depth += 1,
            pulldown_cmark::Event::End(_) => depth -= 1,
            _ => {}
        }
        if opens_block {
            top_open += 1;
        }
        // Leaf events that ARE blocks: a horizontal rule, a display-math block.
        // Both carry no Start/End pair, so close their block on sight.
        let leaf_block = matches!(&ev, pulldown_cmark::Event::Rule | pulldown_cmark::Event::DisplayMath(_));
        buf.push(ev);
        if leaf_block && depth == 0 {
            flush(&mut out, &mut buf, &mut blk_index, true);
        } else if depth == 0 && matches!(buf.last(), Some(pulldown_cmark::Event::End(_))) {
            if top_open > 0 {
                flush(&mut out, &mut buf, &mut blk_index, true);
                top_open -= 1;
            } else {
                flush(&mut out, &mut buf, &mut blk_index, false);
            }
        }
    }
    flush(&mut out, &mut buf, &mut blk_index, false);
    let out = sanitize_html(out);
    let out = resolve_media_urls(out, base);
    let out = upgrade_image_links(out);
    let out = ensure_video_controls(out);
    wrap_tables(out)
}

/// Minimal sanitizer without extra crates: strip <script> blocks + javascript: URLs.
/// Fully byte-based: the input routinely contains multi-byte UTF-8 and
/// string slicing at non-char-boundaries would panic.
fn sanitize_html(s: String) -> String {
    fn find_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        if needle.is_empty() || from >= hay.len() {
            return None;
        }
        hay[from..]
            .windows(needle.len())
            .position(|w| w.eq_ignore_ascii_case(needle))
            .map(|p| from + p)
    }
    let mut b = s.into_bytes();
    loop {
        let Some(start) = find_ci(&b, b"<script", 0) else { break };
        let after_open = b[start..]
            .iter()
            .position(|&c| c == b'>')
            .map(|p| start + p + 1)
            .unwrap_or(b.len());
        match find_ci(&b, b"</script>", after_open.min(b.len())) {
            Some(end_abs) => {
                let end = (end_abs + "</script>".len()).min(b.len());
                // The block wrapper around this content (.md-block, see the
                // renderer) survives stripping, so the editor's block mapping
                // stays aligned even when every byte of a block is dropped.
                b.drain(start..end);
            }
            None => {
                b.truncate(start);
                break;
            }
        }
    }
    // Neutralize javascript: hrefs (case-insensitive, byte-based).
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if i + 11 <= b.len() && b[i..i + 11].eq_ignore_ascii_case(b"javascript:") {
            out.extend_from_slice(b"#blocked:");
            i += 11;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

pub(crate) fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/* ================= media URL resolution + local file serving ================= */
// The view has an opaque origin (with_html), so relative asset refs and local
// paths can never load directly. Every tab carries a `base` (source file's
// parent dir, or the fetched page URL); relative refs are absolutized against
// it, local files are served through http://mdview.localhost/local/.

const DOC_EXTS: &[&str] = &["md", "markdown", "mdown", "mkd", "txt", "text"];
const MAX_MEDIA_BYTES: u64 = 100 * 1024 * 1024;

fn ext_of(path: &str) -> String {
    let clean = path.split(['?', '#']).next().unwrap_or(path);
    clean
        .rsplit(['/', '\\', '.'])
        .next()
        .unwrap_or("")
        .to_lowercase()
}

fn is_doc_path(path: &str) -> bool {
    DOC_EXTS.iter().any(|e| *e == ext_of(path))
}

/// Unambiguous Windows-local: drive-letter path or backslash UNC.
/// (A lone leading `/` is NOT included: against an http base it is a
/// root-absolute URL; `//x` is protocol-relative on the web.)
fn is_windows_abs(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() > 2 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || path.starts_with("\\\\")
}

fn parent_dir_of(path: &str) -> String {
    let clean = path.trim().trim_matches(|c| c == '"' || c == '\'');
    Path::new(clean)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".into())
}

fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Local filesystem URL in the form WebView2 can actually FETCH as a
/// subresource. WebView2 refuses non-standard schemes for <img>/<video>
/// (ERR_UNKNOWN_URL_SCHEME), so wry maps our custom `mdview` protocol to
/// `http://mdview.localhost/...` (its documented workaround) and intercepts
/// that host — emitting plain `mdview://` URLs meant images never loaded.
fn to_local_url(abs_path: &str) -> String {
    let norm = abs_path.replace('\\', "/");
    format!("http://mdview.localhost/local/{}", percent_encode_path(&norm))
}

/// Extract the filesystem path from an mdview://local/... URL in ANY wrapping
/// (mdview://... on some platforms, http://mdview.localhost/... on Windows).
fn decode_local_media_url(url: &str) -> Option<String> {
    let idx = url.find("/local/")?;
    let enc = &url[idx + "/local/".len()..];
    let enc = enc.split(['?', '#']).next().unwrap_or(enc);
    if enc.is_empty() {
        return None;
    }
    let mut path = percent_decode(enc).replace('/', "\\");
    // file:///C:/x style leftovers: strip a single leading slash before drive.
    if path.len() > 3 && path.as_bytes()[0] == b'\\' && path.as_bytes()[2] == b':' {
        path.remove(0);
    }
    // Safety: never serve anything with directory traversal.
    if Path::new(&path)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(path)
}

/// Resolve one raw src/href value against the tab base.
fn resolve_media_ref(raw: &str, base: Option<&str>) -> String {
    let raw_t = raw.trim();
    if raw_t.is_empty() {
        return raw.to_string();
    }
    let low = raw_t.to_lowercase();
    for p in ["http://", "https://", "data:", "blob:", "mdview://", "mdview:"] {
        if low.starts_with(p) {
            return raw.to_string();
        }
    }
    // pulldown-cmark percent-encodes destinations (e.g. `\` -> %5C);
    // undo that first so we resolve (and re-encode) exactly once.
    let r = percent_decode(raw_t);
    if low.starts_with("file:///") {
        let mut p = r[8..].replace('/', "\\");
        if p.len() > 2 && p.as_bytes()[0] == b'\\' && p.as_bytes()[2] == b':' {
            p.remove(0);
        }
        return to_local_url(&p);
    }
    if is_windows_abs(r.as_str()) {
        return to_local_url(&r.replace('/', "\\"));
    }
    // Relative ref (incl. `/root-abs` and `//proto-relative`): needs a base.
    let Some(b) = base.map(str::trim).filter(|s| !s.is_empty()) else {
        return raw.to_string();
    };
    if b.starts_with("http://") || b.starts_with("https://") {
        // url::Url::join handles ./ ../ /root and //proto-relative correctly.
        if let Ok(base_url) = url::Url::parse(b) {
            if let Ok(abs) = base_url.join(&r) {
                return abs.to_string();
            }
        }
        return raw.to_string();
    }
    // Filesystem base dir.
    let rel = r.replace('/', "\\");
    let rel = rel.trim_start_matches('\\');
    to_local_url(&resolve_file_ref(Path::new(b), &rel))
}

/// Resolve a relative ref against a document's directory, falling back to a
/// few parent directories when the ref isn't next to the document. Papers are
/// routinely laid out as `paper/paper.md` + `figures/fig1.png`, so the figures
/// live one level up rather than beside the .md — the most common way a
/// correct-by-sympathy document still shows broken images. The exact join is
/// always tried first, so a correct document is never affected.
fn resolve_file_ref(base_dir: &Path, rel: &str) -> String {
    let direct = normalize_path(&base_dir.join(rel));
    if direct.is_file() {
        return direct.to_string_lossy().into_owned();
    }
    let mut up = base_dir;
    for _ in 0..3 {
        let Some(parent) = up.parent() else { break };
        if parent.as_os_str().is_empty() {
            break;
        }
        up = parent;
        let cand = normalize_path(&up.join(rel));
        if cand.is_file() {
            return cand.to_string_lossy().into_owned();
        }
    }
    direct.to_string_lossy().into_owned()
}

/// Collapse `a/../b` and `.` segments. Written by hand (no `canonicalize`):
/// the file may not exist yet, and canonicalize would also resolve symlinks
/// and force an absolute path. Without this, a plain `../images/x.png` ref —
/// extremely common in papers — would be rejected downstream as a traversal
/// attempt even though it points somewhere entirely legitimate.
fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Rewrite relative src=/href= in rendered HTML against the tab base.
/// Byte-based: values/positions are ASCII-delimited, content is UTF-8.
fn resolve_media_urls(html: String, base: Option<&str>) -> String {
    let b = html.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let mut hit: Option<(&[u8], usize)> = None; // (attr incl. quote, value_start)
        for attr in [b"src=\"".as_slice(), b"href=\"".as_slice(), b"src='".as_slice(), b"href='".as_slice()] {
            if b[i..].starts_with(attr) {
                hit = Some((attr, i + attr.len()));
                break;
            }
        }
        if let Some((attr, vs)) = hit {
            let q = *attr.last().unwrap();
            if let Some(rel) = b[vs..].iter().position(|&c| c == q) {
                let raw = String::from_utf8_lossy(&b[vs..vs + rel]).to_string();
                // Only touch plausible URL-ish values, not in-page anchors.
                let resolved = if raw.starts_with('#') {
                    raw
                } else {
                    let resolved = resolve_media_ref(&raw, base);
                    // Every remote image goes through the app's fetch handler
                    // (Chrome TLS/HTTP2 impersonation, content-type check) —
                    // extensionless and Cloudflare-gated URLs included. This
                    // also avoids double-loading: WebView2 only ever sees the
                    // mdview.localhost URL, never the original.
                    let is_src = attr == b"src=\"".as_slice() || attr == b"src='".as_slice();
                    if is_src && is_http_url(&resolved) && !is_mdview_serve(&resolved) {
                        to_fetch_url(&resolved)
                    } else {
                        resolved
                    }
                };
                out.extend_from_slice(attr);
                out.extend_from_slice(resolved.as_bytes());
                out.push(q);
                i = vs + rel + 1;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or(html)
}

/// Wrap every <table> in a scroll container so a wide table can scroll
/// horizontally without the page itself ever scrolling sideways.
///
/// This is deliberately done here, at render time, rather than by measuring in
/// the frontend: intrinsic width is a layout property, and guessing it in JS
/// meant a measure/mutate/re-measure pass on every render, tab switch, zoom and
/// window resize. Emitting the wrapper as part of the HTML makes it a plain
/// structural guarantee instead.
///
/// Byte-based, like the other passes, because the input is arbitrary UTF-8 and
/// slicing at a non-char boundary would panic. Tracks nesting depth so a table
/// inside a cell would be wrapped correctly rather than truncated.
fn wrap_tables(html: String) -> String {
    let b = html.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len() + 64);
    let mut i = 0;
    // Stack of open <table> depths: how many wrappers are currently unclosed.
    let mut open: usize = 0;
    while i < b.len() {
        if b[i] == b'<' {
            if b[i..].starts_with(b"<table") {
                // Only a real tag, not a prefix like <tablet>
                let boundary = b.get(i + 6).map_or(true, |&c| c == b'>' || c == b' ' || c == b'/');
                if boundary {
                    out.extend_from_slice(b"<div class=\"tw\"><table");
                    open += 1;
                    i += 6;
                    continue;
                }
            } else if b[i..].starts_with(b"</table>") {
                if open > 0 {
                    out.extend_from_slice(b"</table></div>");
                    open -= 1;
                } else {
                    out.extend_from_slice(b"</table>");
                }
                i += 8;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or(html)
}

/// GitHub READMEs embed <video> without controls (black box) — fix that.
/// Byte-based for the same UTF-8 safety reason.
fn ensure_video_controls(html: String) -> String {
    let b = html.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"<video") {
            if let Some(rel) = b[i..].iter().position(|&c| c == b'>') {
                let tag = &b[i..i + rel];
                let has_controls = tag
                    .windows(8)
                    .any(|w| w.eq_ignore_ascii_case(b"controls"));
                if has_controls {
                    out.extend_from_slice(tag);
                    out.push(b'>');
                } else {
                    out.extend_from_slice(b"<video controls preload=\"metadata\"");
                    out.extend_from_slice(&tag["<video".len()..]);
                    out.push(b'>');
                }
                i += rel + 1;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or(html)
}

fn media_content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => return None,
    })
}

fn media_response(status: u16, ctype: &str, body: Vec<u8>) -> http::Response<Cow<'static, [u8]>> {
    let status = http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::OK);
    http::Response::builder()
        .status(status)
        .header("Content-Type", ctype)
        .header("Access-Control-Allow-Origin", "*")
        .body(Cow::Owned(body))
        .unwrap()
}

/// True for URLs the app serves itself (assets, local media, remote fetch).
/// Never re-route those into the fetch handler (that would wrap a fetch URL
/// inside another fetch URL).
fn is_mdview_serve(s: &str) -> bool {
    s.starts_with("http://mdview.localhost/") || s.starts_with("mdview.localhost/")
}

/* ===== Remote images: Chrome TLS/HTTP2 impersonation via wreq =====
   Remote <img> targets are rewritten to http://mdview.localhost/fetch?url=…
   and WebView2 only ever sees that URL (no double loading). The handler
   below fetches the original with wreq (BoringSSL, Chrome emulation) — this
   is what lets extensionless and Cloudflare-gated image URLs through —
   verifies the response really is an image, and streams it back.

   Two shared singletons instead of per-request state: one tokio runtime
   (2 worker threads) and two clients (30s for images, 4s for link probes).
   wreq is async-only; block_on() from the protocol-handler threads is what
   wry's background thread pool calls us on. */
fn remote_runtime() -> &'static tokio::runtime::Runtime {
    static REMOTE_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    REMOTE_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("remote fetch tokio runtime must build")
    })
}

fn remote_client() -> Result<&'static wreq::Client, String> {
    static REMOTE_CLIENT: OnceLock<wreq::Client> = OnceLock::new();
    if let Some(c) = REMOTE_CLIENT.get() {
        return Ok(c);
    }
    let c = wreq::Client::builder()
        .emulation(wreq_util::Emulation::Chrome147)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("client build error: {e}"))?;
    Ok(REMOTE_CLIENT.get_or_init(|| c))
}

fn probe_client() -> Result<&'static wreq::Client, String> {
    static PROBE_CLIENT: OnceLock<wreq::Client> = OnceLock::new();
    if let Some(c) = PROBE_CLIENT.get() {
        return Ok(c);
    }
    let c = wreq::Client::builder()
        .emulation(wreq_util::Emulation::Chrome147)
        .timeout(Duration::from_secs(4))
        .build()
        .map_err(|e| format!("client build error: {e}"))?;
    Ok(PROBE_CLIENT.get_or_init(|| c))
}

/// Send a GET/HEAD with manual redirect handling: up to 5 hops, each joined
/// against the previous URL (relative Location headers included). wreq does
/// not follow redirects out of the box, and image CDNs love them.
async fn send_with_redirects(client: &wreq::Client, url: &str, head: bool) -> Result<wreq::Response, String> {
    let mut current = url.to_string();
    for _ in 0..6 {
        let resp = if head {
            client.head(&current).send().await
        } else {
            client.get(&current).send().await
        }
        .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        if status.as_u16() >= 300 && status.as_u16() < 400 {
            let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()) else {
                return Ok(resp);
            };
            let next = url::Url::parse(&current)
                .and_then(|base| base.join(loc))
                .map(|u| u.to_string())
                .map_err(|e| format!("redirect join failed: {e}"))?;
            current = next;
            continue;
        }
        return Ok(resp);
    }
    Err("too many redirects".to_string())
}

/// Fetch a remote image with Chrome impersonation. Single GET (redirects
/// aside) — the response headers are checked *before* the body is read, so a
/// non-image response is rejected without downloading anything.
fn fetch_remote_image(url: &str) -> Result<(String, Vec<u8>), String> {
    let client = remote_client()?;
    remote_runtime().block_on(async move {
        let resp = send_with_redirects(client, url, false).await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !ctype.starts_with("image/") {
            return Err(format!("not an image: {ctype}"));
        }
        if let Some(len) = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
        {
            if len > MAX_MEDIA_BYTES {
                return Err(format!("too large: {len} bytes"));
            }
        }
        let bytes = resp.bytes().await.map_err(|e| format!("read failed: {e}"))?;
        if bytes.len() as u64 > MAX_MEDIA_BYTES {
            return Err(format!("too large: {} bytes", bytes.len()));
        }
        let mime = ctype.split(';').next().unwrap_or(&ctype).trim().to_string();
        Ok((mime, bytes.to_vec()))
    })
}

/// Create a fetch URL for the protocol handler.
fn to_fetch_url(url: &str) -> String {
    format!("http://mdview.localhost/fetch?url={}", percent_encode_path(url))
}

/// Decode a fetch URL to get the original URL.
fn decode_fetch_url(url: &str) -> Option<String> {
    let idx = url.find("/fetch?url=")?;
    let encoded = &url[idx + "/fetch?url=".len()..];
    let encoded = encoded.split(['?', '#']).next().unwrap_or(encoded);
    if encoded.is_empty() {
        return None;
    }
    Some(percent_decode(encoded))
}

/// Serve remote media from a /fetch?url=... URL. Any failure — DNS, HTTP,
/// wrong content-type, too large — returns 404, never a partially served
/// file and never a guess.
fn serve_remote_media(uri: &str) -> Option<http::Response<Cow<'static, [u8]>>> {
    let fail = || media_response(404, "text/plain", b"not found".to_vec());
    let url = decode_fetch_url(uri)?;
    match fetch_remote_image(&url) {
        Ok((ctype, bytes)) => Some(media_response(200, &ctype, bytes)),
        Err(_) => Some(fail()),
    }
}

/// HEAD-probe an ambiguous link: Some(content-type) when the server answers
/// image/*. Network access is skipped entirely when LINK_PROBE_NETWORK is
/// false (unit tests), and a probe failure/timeout means "keep the link".
static LINK_PROBE_NETWORK: AtomicBool = AtomicBool::new(true);

fn probe_remote_link(url: &str) -> Option<String> {
    if !LINK_PROBE_NETWORK.load(Ordering::Relaxed) {
        return None;
    }
    let client = probe_client().ok()?;
    remote_runtime().block_on(async move {
        let resp = send_with_redirects(client, url, true).await.ok()?;
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ctype.starts_with("image/") {
            Some(ctype.split(';').next().unwrap_or("").trim().to_string())
        } else {
            None
        }
    })
}

/// Markdown links whose target *is* an image must render as one (extension
/// counts for `.png`-style URLs; otherwise a HEAD content-type probe decides).
/// Everything else stays a link. Media wrappers (a link around an <img> or
/// <video>) are never upgraded — that markup is deliberate.
#[derive(Clone, Copy, PartialEq)]
enum LinkKind {
    Image,
    NotImage,
    Unknown,
}

fn link_kind_of(url: &str) -> LinkKind {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or(path);
    match ext_of(last).as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "avif" => LinkKind::Image,
        "html" | "htm" | "xhtml" | "pdf" | "mp4" | "m4v" | "webm" | "mov" | "ogv" | "mp3" | "wav"
        | "ogg" | "zip" | "tar" | "gz" | "rar" | "7z" | "exe" | "msi" | "css" | "js" | "mjs" | "cjs"
        | "json" | "xml" | "csv" | "md" | "markdown" | "txt" | "yaml" | "yml" | "toml" | "wasm"
        | "woff" | "woff2" | "ttf" | "otf" | "dmg" | "iso" | "apk" | "deb" | "rpm" => {
            LinkKind::NotImage
        }
        _ => LinkKind::Unknown,
    }
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch == '>' {
            in_tag = false;
            continue;
        }
        if !in_tag {
            out.push(ch);
        }
    }
    out
}

/// Decode HTML entities in an attribute value. pulldown-cmark escapes `&`
/// as `&amp;` in emitted attributes; consumers that want the raw URL (probes,
/// to_fetch_url) must unescape, or `&amp;` lands inside the query string.
fn html_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(end) = s[i..].find(';') {
                let e = i + end;
                if e - i <= 10 {
                    let entity = &s[i + 1..e];
                    let repl = match entity {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => entity.strip_prefix('#').and_then(|num| {
                            let n = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                                u32::from_str_radix(hex, 16).ok()
                            } else {
                                num.parse::<u32>().ok()
                            }?;
                            char::from_u32(n)
                        }),
                    };
                    if let Some(c) = repl {
                        out.push(c);
                        i = e + 1;
                        continue;
                    }
                }
            }
        }
        let next = s[i..].find('&').map(|r| i + r).unwrap_or(s.len());
        out.push_str(&s[i..next]);
        i = next;
    }
    out
}

/// Find `name="value"` (or `name='value'`) inside a single HTML tag,
/// case-insensitive on the attribute name. The name must not be a suffix of
/// a longer attribute (data-href is not href).
fn find_attr_value(tag: &str, name: &str) -> Option<String> {
    let low = tag.to_ascii_lowercase();
    let bytes = low.as_bytes();
    let name_char = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b':';
    let mut from = 0;
    while let Some(rel) = low[from..].find(name) {
        let at = from + rel;
        if at > 0 && name_char(bytes[at - 1]) {
            from = at + name.len();
            continue;
        }
        let mut j = at + name.len();
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
            j += 1;
        }
        if bytes.get(j) != Some(&b'=') {
            from = at + name.len();
            continue;
        }
        j += 1;
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
            j += 1;
        }
        let quote = *bytes.get(j)?;
        if quote != b'"' && quote != b'\'' {
            from = at + name.len();
            continue;
        }
        let value_start = j + 1;
        let value_len = low[value_start..].find(quote as char)?;
        return Some(html_unescape(&tag[value_start..value_start + value_len]));
    }
    None
}

/// One `<a ...>inner</a>` occurrence in the rendered HTML.
struct AnchorOccurrence {
    /// Byte range of the whole element (from '<' of the anchor to "</a>").
    start: usize,
    end: usize,
    href: String,
    inner: String,
}

fn find_anchors(html: &str) -> Vec<AnchorOccurrence> {
    let b = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == b'<'
            && b[i + 1] == b'a'
            && matches!(b.get(i + 2), Some(b' ' | b'\t' | b'\n' | b'\r' | b'>'))
        {
            // End of the opening tag: first unquoted '>'.
            let mut j = i + 2;
            let mut quote: u8 = 0;
            while j < b.len() {
                let c = b[j];
                if quote != 0 {
                    if c == quote {
                        quote = 0;
                    }
                } else if c == b'"' || c == b'\'' {
                    quote = c;
                } else if c == b'>' {
                    break;
                }
                j += 1;
            }
            if j >= b.len() {
                break;
            }
            let tag_end = j + 1;
            let tag = &html[i..tag_end];
            // Anchors don't nest in HTML5: the first "</a>" closes this one.
            let Some(close_rel) = html[tag_end..].find("</a>") else {
                i = tag_end;
                continue;
            };
            let inner = &html[tag_end..tag_end + close_rel];
            let href = find_attr_value(tag, "href").unwrap_or_default();
            out.push(AnchorOccurrence {
                start: i,
                end: tag_end + close_rel + "</a>".len(),
                href,
                inner: inner.to_string(),
            });
            i = tag_end + close_rel + "</a>".len();
            continue;
        }
        i += 1;
    }
    out
}

/// Upgrade `<a href="…">text</a>` to `<img>` when the target is an image.
/// Probes happen in parallel on the shared runtime, capped at 16 ambiguous
/// URLs per document; each probe has a 4s ceiling inside probe_client().
/// On any doubt the anchor is left alone — never guess.
fn upgrade_image_links(html: String) -> String {
    let anchors = find_anchors(&html);
    if anchors.is_empty() {
        return html;
    }
    // Decide per anchor.
    let mut decisions: Vec<Option<bool>> = Vec::with_capacity(anchors.len()); // true = upgrade
    let mut probe_idx: Vec<usize> = Vec::new();
    for a in &anchors {
        let inner_too_big = a.inner.len() > 400 || a.inner.contains('<');
        if !is_http_url(&a.href) || inner_too_big {
            decisions.push(Some(false));
            continue;
        }
        match link_kind_of(&a.href) {
            LinkKind::Image => {
                decisions.push(Some(true));
            }
            LinkKind::NotImage => {
                decisions.push(Some(false));
            }
            LinkKind::Unknown => {
                decisions.push(None);
                probe_idx.push(decisions.len() - 1);
            }
        }
    }
    // Ambiguous URLs: probe in parallel, capped.
    let probes: Vec<(usize, &str)> = probe_idx
        .iter()
        .take(16)
        .map(|&idx| (idx, anchors[idx].href.as_str()))
        .collect();
    let mut results: std::collections::HashMap<usize, bool> =
        std::collections::HashMap::with_capacity(probes.len());
    if !probes.is_empty() {
        let mut handles = Vec::with_capacity(probes.len());
        for (idx, url) in probes {
            let url = url.to_string();
            handles.push(std::thread::spawn(move || (idx, probe_remote_link(&url))));
        }
        for h in handles {
            if let Ok((idx, outcome)) = h.join() {
                results.insert(idx, outcome.is_some());
            }
        }
    }
    for &idx in &probe_idx {
        decisions[idx] = Some(results.get(&idx).copied().unwrap_or(false));
    }
    // Rebuild the document in one pass over segments.
    let mut out = String::with_capacity(html.len() + 64);
    let mut cursor = 0usize;
    for (idx, a) in anchors.iter().enumerate() {
        if decisions[idx] != Some(true) {
            continue;
        }
        out.push_str(&html[cursor..a.start]);
        let alt = strip_tags(&a.inner);
        let alt = alt.trim();
        let alt: String = alt
            .chars()
            .take(160)
            .map(|c| if c == '"' { '\'' } else { c })
            .collect();
        out.push_str("<img src=\"");
        out.push_str(&to_fetch_url(&a.href));
        out.push_str("\" alt=\"");
        out.push_str(&alt);
        out.push_str("\">");
        cursor = a.end;
    }
    if cursor == 0 {
        return html;
    }
    out.push_str(&html[cursor..]);
    out
}

/// Serve the app's own vendored assets (KaTeX: JS, CSS, fonts) over the same
/// custom protocol. The view has an opaque origin, so a bare `mdview://` src
/// is refused by WebView2 — assets use the http://mdview.localhost/asset/...
/// form, which this handler owns. Everything is compiled into the binary, so
/// math renders with no network at all.
/// Gzip wrapper around a raw DEFLATE stream: 10-byte header, deflate body,
/// 8-byte trailer. We verify the magic, skip the header, and let miniz stop at
/// the end of the stream (the CRC/ISIZE trailer is simply trailing bytes we
/// never look at — integrity is guaranteed by the test suite round-tripping
/// every vendored blob, not by re-checking the CRC at runtime).
fn gunzip(gz: &[u8]) -> Option<Vec<u8>> {
    if gz.len() < 18 || gz[0] != 0x1f || gz[1] != 0x8b {
        return None;
    }
    miniz_oxide::inflate::decompress_to_vec(&gz[10..]).ok()
}

/// Memoized decompression cache. Holds at most one inflated asset: the large
/// one (mermaid) is expensive to re-inflate on every diagram, but we refuse to
/// keep it resident longer than needed. `None` means nothing is cached, which is
/// the state for the entire life of a session that never opens a diagram.
static ASSET_CACHE: Mutex<Option<(String, Arc<Vec<u8>>)>> = Mutex::new(None);

/// Serve `name` from a gzipped vendor blob, inflating (and memoizing) on first
/// use. The cache keeps a single entry; requesting a different asset replaces
/// it, so peak extra memory is one inflated script rather than every asset
/// ever touched.
fn gz_asset(name: &str, gz: &'static [u8], ctype: &'static str) -> Option<http::Response<Cow<'static, [u8]>>> {
    if let Ok(guard) = ASSET_CACHE.lock() {
        if let Some((cached_name, body)) = guard.as_ref() {
            if cached_name == name {
                return Some(media_response(200, ctype, body.as_ref().clone()));
            }
        }
    }
    let inflated = gunzip(gz)?;
    let body = Arc::new(inflated);
    if let Ok(mut guard) = ASSET_CACHE.lock() {
        *guard = Some((name.to_string(), Arc::clone(&body)));
    }
    Some(media_response(200, ctype, body.as_ref().clone()))
}

/// Drop the inflated-asset cache. Called when the page reports that no diagram
/// or math document is open any more, so a long reading session returns to its
/// near-idle footprint.
fn release_asset_cache() {
    if let Ok(mut guard) = ASSET_CACHE.lock() {
        *guard = None;
    }
}

/// Serve the app's own vendored assets (KaTeX: JS, CSS, fonts; mermaid: JS)
/// over the same custom protocol. The view has an opaque origin, so a bare
/// `mdview://` src is refused by WebView2 — assets use the
/// http://mdview.localhost/asset/... form, which this handler owns.
/// Everything is compiled into the binary, so math and diagrams render with no
/// network at all. JS/CSS are stored gzipped (see tools/vendor-assets.py).
fn serve_app_asset(uri: &str) -> Option<http::Response<Cow<'static, [u8]>>> {
    let rest = uri.split_once("/asset/")?.1;
    let name = rest.split(['?', '#']).next().unwrap_or(rest);
    // Fonts are referenced relatively by the stylesheet, and the page has no
    // base URL to resolve them against — so hand out absolute paths.
    if let Some(f) = name.strip_prefix("katex/fonts/") {
        let body: &'static [u8] = match f {
            "KaTeX_AMS-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_AMS-Regular.woff2").as_slice(),
            "KaTeX_Caligraphic-Bold.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Caligraphic-Bold.woff2").as_slice(),
            "KaTeX_Caligraphic-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Caligraphic-Regular.woff2").as_slice(),
            "KaTeX_Fraktur-Bold.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Fraktur-Bold.woff2").as_slice(),
            "KaTeX_Fraktur-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Fraktur-Regular.woff2").as_slice(),
            "KaTeX_Main-Bold.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Main-Bold.woff2").as_slice(),
            "KaTeX_Main-BoldItalic.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Main-BoldItalic.woff2").as_slice(),
            "KaTeX_Main-Italic.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Main-Italic.woff2").as_slice(),
            "KaTeX_Main-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Main-Regular.woff2").as_slice(),
            "KaTeX_Math-BoldItalic.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Math-BoldItalic.woff2").as_slice(),
            "KaTeX_Math-Italic.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Math-Italic.woff2").as_slice(),
            "KaTeX_SansSerif-Bold.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_SansSerif-Bold.woff2").as_slice(),
            "KaTeX_SansSerif-Italic.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_SansSerif-Italic.woff2").as_slice(),
            "KaTeX_SansSerif-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_SansSerif-Regular.woff2").as_slice(),
            "KaTeX_Script-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Script-Regular.woff2").as_slice(),
            "KaTeX_Size1-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Size1-Regular.woff2").as_slice(),
            "KaTeX_Size2-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Size2-Regular.woff2").as_slice(),
            "KaTeX_Size3-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Size3-Regular.woff2").as_slice(),
            "KaTeX_Size4-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Size4-Regular.woff2").as_slice(),
            "KaTeX_Typewriter-Regular.woff2" => include_bytes!("../frontend/vendor/katex/fonts/KaTeX_Typewriter-Regular.woff2").as_slice(),
            _ => return None,
        };
        return Some(media_response(200, "font/woff2", body.to_vec()));
    }
    match name {
        "katex/katex.min.js" => gz_asset(
            name,
            include_bytes!("../frontend/vendor/katex/katex.min.js.gz"),
            "text/javascript",
        ),
        "mermaid/mermaid.min.js" => gz_asset(
            name,
            include_bytes!("../frontend/vendor/mermaid/mermaid.min.js.gz"),
            "text/javascript",
        ),
        "codemirror/codemirror.min.js" => gz_asset(
            name,
            include_bytes!("../frontend/vendor/codemirror.min.js.gz"),
            "text/javascript",
        ),
        "katex/katex.min.css" => {
            // Fonts are referenced relatively by the stylesheet, and the page
            // has no base URL to resolve them against, so they must be absolute.
            let css = String::from_utf8(gunzip(include_bytes!(
                "../frontend/vendor/katex/katex.min.css.gz"
            ))?)
            .ok()?
            .replace(
                "url(fonts/",
                "url(http://mdview.localhost/asset/katex/fonts/",
            );
            Some(media_response(200, "text/css", css.into_bytes()))
        }
        _ => None,
    }
}

/// Serve local media from a /local/ URL (http://mdview.localhost form):
/// images/video/audio only, size-capped.
fn serve_local_media(uri: &str) -> http::Response<Cow<'static, [u8]>> {
    let fail = || media_response(404, "text/plain", b"not found".to_vec());
    let Some(path) = decode_local_media_url(uri) else {
        return fail();
    };
    let ctype = match media_content_type(&ext_of(&path)) {
        Some(c) => c,
        None => return fail(),
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) if m.is_file() && m.len() <= MAX_MEDIA_BYTES => m,
        _ => return fail(),
    };
    let _ = meta;
    match std::fs::read(&path) {
        Ok(bytes) if (bytes.len() as u64) <= MAX_MEDIA_BYTES => media_response(200, ctype, bytes),
        _ => fail(),
    }
}

/* ================= IPC: JS -> Rust ================= */

/// Lock the process-wide document store. The DB is one connection behind a
/// mutex; every command is local and indexed, so reads are microseconds and a
/// save is low milliseconds.
fn db_lock() -> Result<std::sync::MutexGuard<'static, db::Store>, String> {
    db::store().and_then(|m| m.lock().map_err(|_| "document database lock poisoned".to_string()))
}

fn handle_ipc(body: &str, proxy: &tao::event_loop::EventLoopProxy<UserEvent>) {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return,
    };
    // `id` is the bridge's request-correlation key — it is NOT a document id.
    // Payload keys must never carry `id` (see `doc_id` below); if one ever
    // does, the answer is routed to a nonexistent pending entry and the JS
    // promise hangs with no error. Keep this strict.
    let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("").to_string();
    let id = v.get("id").and_then(|c| c.as_str()).unwrap_or("").to_string();
    match cmd.as_str() {
        "render" => {
            // Typical pastes render in well under a millisecond; answer inline.
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let base = v.get("base").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let base_opt = if base.trim().is_empty() { None } else { Some(base.as_str()) };
            let html = render_markdown_to_html(&text, base_opt);
            let js = format!(
                "window.__mdview_ipc_result && window.__mdview_ipc_result({}, true, {});",
                serde_json::to_string(&id).unwrap_or("\"\"".into()),
                serde_json::to_string(&html).unwrap_or("\"\"".into())
            );
            let _ = proxy.send_event(UserEvent::IpcEval(js));
        }
        "blocks" => {
            // Source line extents of every top-level markdown block, for the
            // editor's click-to-edit mapping. Line coordinates, not bytes —
            // JS pages are UTF-16 and will do its own offset math.
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let truncated = truncate_str(&text, MAX_MD_BYTES);
            let spans = block_spans(truncated);
            let json = serde_json::to_string(&spans).unwrap_or_else(|_| "[]".into());
            let js = format!(
                "window.__mdview_ipc_result && window.__mdview_ipc_result({}, true, {});",
                serde_json::to_string(&id).unwrap_or("\"\"".into()),
                json
            );
            let _ = proxy.send_event(UserEvent::IpcEval(js));
        }
        "read_file" => {
            let path = v.get("path").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let p2 = proxy.clone();
            std::thread::spawn(move || {
                send_ipc_result(&p2, &id, read_file_limited(&path, MAX_MD_BYTES));
            });
        }
        "fetch_url" => {
            let url = v.get("url").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let p2 = proxy.clone();
            std::thread::spawn(move || {
                send_ipc_result(&p2, &id, fetch_url_text(&url));
            });
        }
        "fullscreen" => {
            let _ = proxy.send_event(UserEvent::ToggleFullscreen(id));
        }
        "release_asset" => {
            // Nothing diagram- or math-shaped is open any more: drop the
            // inflated asset so idle memory returns to baseline.
            release_asset_cache();
        }
        "open_path" => {
            // Clicked a local file link (http://mdview.localhost/local/... —
            // JS already preventDefaulted).
            // Docs open as a new tab; media opens in the default app.
            let url = v.get("url").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let p2 = proxy.clone();
            std::thread::spawn(move || {
                let res: Result<String, String> = (|| {
                    let path = decode_local_media_url(&url).ok_or("bad link".to_string())?;
                    if is_doc_path(&path) {
                        let text = read_file_limited(&path, MAX_MD_BYTES)?;
                        Ok(serde_json::to_string(&json!({
                            "text": text,
                            "name": short_name(&path),
                            "base": parent_dir_of(&path),
                            "source": path,
                        }))
                        .unwrap_or_default())
                    } else {
                        open_external(&path);
                        Ok("{\"opened\":true}".to_string())
                    }
                })();
                send_ipc_result(&p2, &id, res);
            });
        }
        "clipboard_files" => {
            // CF_HDROP: real paths of files copied in Explorer — the only
            // way a pasted file can bring its directory (and images) along.
            let p2 = proxy.clone();
            std::thread::spawn(move || {
                send_ipc_result(&p2, &id, clipboard_docs_json());
            });
        }
        "clipboard_text" => {
            // WebView2's navigator.clipboard.read() is permission-blocked in
            // the app, so the Paste button reads the clipboard via Win32 in
            // this process instead (same pattern as clipboard_files).
            let p2 = proxy.clone();
            std::thread::spawn(move || {
                send_ipc_result(&p2, &id, clipboard_text());
            });
        }
        "pick_file" => {
            // Needs the UI thread: modal dialog, owner window, z-order.
            let _ = proxy.send_event(UserEvent::PickFile { id });
        }
        "save_prefs" => {
            let payload = serde_json::to_string(&v).unwrap_or_default();
            save_prefs_json(&payload);
            let js = format!(
                "window.__mdview_ipc_result && window.__mdview_ipc_result({}, true, true);",
                serde_json::to_string(&id).unwrap_or("\"\"".into())
            );
            let _ = proxy.send_event(UserEvent::IpcEval(js));
        }
        "save_document" | "auto_save_document" => {
            // The bridge reserves `id` for request correlation — payload keys
            // must never carry it. Document ids travel as `doc_id`.
            let doc_id = v.get("doc_id").and_then(|x| x.as_i64());
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let base = v.get("base").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let source_kind = v.get("source_kind").and_then(|x| x.as_str()).unwrap_or("paste").to_string();
            let source_ref = v.get("source_ref").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let res = db_lock()
                .and_then(|s| {
                    let r = if cmd == "auto_save_document" {
                        s.auto_save(&title, &text, &base, &source_kind, &source_ref)
                    } else {
                        s.save(doc_id, &title, &text, &base, &source_kind, &source_ref)
                    };
                    r.map_err(|e| e.to_string())
                })
                .and_then(|d| serde_json::to_string(&d).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "open_document" => {
            let doc_id = v.get("doc_id").and_then(|x| x.as_i64()).unwrap_or(-1);
            let res = db_lock()
                .and_then(|s| s.open_document(doc_id).map_err(|e| e.to_string()))
                .and_then(|d| serde_json::to_string(&d).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "search_documents" => {
            let query = v.get("query").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let limit = v
                .get("limit")
                .and_then(|x| x.as_u64())
                .unwrap_or(db::DEFAULT_SEARCH_LIMIT as u64) as usize;
            let res = db_lock()
                .and_then(|s| s.search(&query, limit).map_err(|e| e.to_string()))
                .and_then(|hits| serde_json::to_string(&hits).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "recent_documents" => {
            let limit = v
                .get("limit")
                .and_then(|x| x.as_u64())
                .unwrap_or(db::DEFAULT_RECENT_LIMIT as u64) as usize;
            let res = db_lock()
                .and_then(|s| s.recent(limit).map_err(|e| e.to_string()))
                .and_then(|docs| serde_json::to_string(&docs).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "rename_document" => {
            let doc_id = v.get("doc_id").and_then(|x| x.as_i64()).unwrap_or(-1);
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let res = db_lock()
                .and_then(|s| s.rename(doc_id, &title).map_err(|e| e.to_string()))
                .and_then(|ok| serde_json::to_string(&ok).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "delete_document" => {
            let doc_id = v.get("doc_id").and_then(|x| x.as_i64()).unwrap_or(-1);
            let res = db_lock()
                .and_then(|s| s.delete(doc_id).map_err(|e| e.to_string()))
                .and_then(|ok| serde_json::to_string(&ok).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "annotation_list" => {
            let doc_id = v.get("doc_id").and_then(|x| x.as_i64()).unwrap_or(-1);
            let res = db_lock()
                .and_then(|s| s.annotation_list(doc_id).map_err(|e| e.to_string()))
                .and_then(|rows| serde_json::to_string(&rows).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        // The bridge reserves `id` for request correlation — annotation ids
        // travel as `annotation_id`, never `id`.
        "annotation_add" => {
            let doc_id = v.get("doc_id").and_then(|x| x.as_i64()).unwrap_or(-1);
            let exact = v.get("exact").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let prefix = v.get("prefix").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let suffix = v.get("suffix").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let start = v.get("start").and_then(|x| x.as_i64()).unwrap_or(-1);
            let end = v.get("end").and_then(|x| x.as_i64()).unwrap_or(-1);
            let color = v.get("color").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let note = v.get("note").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let res = db_lock()
                .and_then(|s| {
                    s.annotation_add(doc_id, &exact, &prefix, &suffix, start, end, &color, &note)
                        .map_err(|e| e.to_string())
                })
                .and_then(|a| serde_json::to_string(&a).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "annotation_update" => {
            let aid = v.get("annotation_id").and_then(|x| x.as_i64()).unwrap_or(-1);
            let color = v.get("color").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let note = v.get("note").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let res = db_lock()
                .and_then(|s| s.annotation_update(aid, &color, &note).map_err(|e| e.to_string()))
                .and_then(|ok| serde_json::to_string(&ok).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "annotation_refresh_anchor" => {
            // System path: re-resolved offsets/context/state after a re-render.
            // `exact` is not accepted — the stored quote is never rewritten.
            let aid = v.get("annotation_id").and_then(|x| x.as_i64()).unwrap_or(-1);
            let prefix = v.get("prefix").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let suffix = v.get("suffix").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let start = v.get("start").and_then(|x| x.as_i64()).unwrap_or(-1);
            let end = v.get("end").and_then(|x| x.as_i64()).unwrap_or(-1);
            let state = v.get("state").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let res = db_lock()
                .and_then(|s| {
                    s.annotation_refresh_anchor(aid, &prefix, &suffix, start, end, &state)
                        .map_err(|e| e.to_string())
                })
                .and_then(|ok| serde_json::to_string(&ok).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "annotation_delete" => {
            let aid = v.get("annotation_id").and_then(|x| x.as_i64()).unwrap_or(-1);
            let res = db_lock()
                .and_then(|s| s.annotation_delete(aid).map_err(|e| e.to_string()))
                .and_then(|ok| serde_json::to_string(&ok).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "find_similar" => {
            let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let source_ref = v.get("source_ref").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let res = db_lock()
                .and_then(|s| s.find_similar(&text, &source_ref).map_err(|e| e.to_string()))
                .and_then(|m| serde_json::to_string(&m).map_err(|e| e.to_string()));
            send_ipc_result(&proxy, &id, res);
        }
        "set_title" => {
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let _ = proxy.send_event(UserEvent::SetTitle(title));
            let js = format!(
                "window.__mdview_ipc_result && window.__mdview_ipc_result({}, true, true);",
                serde_json::to_string(&id).unwrap_or("\"\"".into())
            );
            let _ = proxy.send_event(UserEvent::IpcEval(js));
        }
        _ => {}
    }
}

fn send_ipc_result(
    proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
    id: &str,
    res: Result<String, String>,
) {
    let js = match res {
        Ok(text) => format!(
            "window.__mdview_ipc_result && window.__mdview_ipc_result({}, true, {});",
            serde_json::to_string(&id).unwrap_or("\"\"".into()),
            serde_json::to_string(&text).unwrap_or("\"\"".into())
        ),
        Err(e) => format!(
            "window.__mdview_ipc_result && window.__mdview_ipc_result({}, false, {});",
            serde_json::to_string(&id).unwrap_or("\"\"".into()),
            serde_json::to_string(&e).unwrap_or("\"error\"".into())
        ),
    };
    let _ = proxy.send_event(UserEvent::IpcEval(js));
}

fn read_file_limited(path: &str, max: usize) -> Result<String, String> {
    let clean = path.trim().trim_matches(|c| c == '"' || c == '\'');
    let p = PathBuf::from(clean);
    let meta = std::fs::metadata(&p).map_err(|e| format!("cannot stat file: {e}"))?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    if meta.len() > max as u64 + 1024 {
        return Err(format!("file too large (> {} MB)", max / 1024 / 1024));
    }
    let f = std::fs::File::open(&p).map_err(|e| format!("cannot open: {e}"))?;
    let mut buf = Vec::new();
    f.take(max as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read: {e}"))?;
    // Binary guard: NUL byte in first 8KB => almost certainly not markdown.
    if buf.iter().take(8192).any(|&b| b == 0) {
        return Err("binary file — not markdown".into());
    }
    String::from_utf8(buf).map_err(|_| "file is not valid UTF-8 text".to_string())
}

fn fetch_url_text(url: &str) -> Result<String, String> {
    let url = url.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http(s) URLs are fetched".into());
    }
    let mut resp = ureq::get(&url)
        .header("User-Agent", "mdview/0.1")
        .header("Accept", "text/markdown,text/plain,text/*;q=0.9,*/*;q=0.5")
        .call()
        .map_err(|e| format!("fetch failed: {e}"))?;
    if let Some(len) = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        if len > MAX_FETCH_BYTES + 1024 {
            return Err(format!("URL too large (> {} MB)", MAX_FETCH_BYTES / 1024 / 1024));
        }
    }
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read failed: {e}"))?;
    if body.len() > MAX_FETCH_BYTES {
        return Err(format!("URL too large (> {} MB)", MAX_FETCH_BYTES / 1024 / 1024));
    }
    if body.as_bytes().iter().take(8192).any(|&b| b == 0) {
        return Err("URL did not return text".into());
    }
    Ok(body)
}

/* ================= startup docs / open args ================= */

struct InitialDoc {
    name: String,
    text: String,
    base: String,
    /// Full source path/URL, when the document came from one. The internal
    /// database uses it as a stable alias for the same file or page.
    source: String,
}

fn resolve_open_arg(arg: &str) -> InitialDoc {
    let t = arg.trim().trim_matches(|c| c == '"' || c == '\'');
    if t.starts_with("http://") || t.starts_with("https://") {
        return match fetch_url_text(t) {
            Ok(text) => InitialDoc { name: short_name(t), text, base: t.to_string(), source: t.to_string() },
            Err(e) => InitialDoc {
                name: "error".into(),
                text: format!("# Could not fetch URL\n\n`{}`\n\n{}", t, e),
                base: String::new(),
                source: String::new(),
            },
        };
    }
    let p = PathBuf::from(t);
    if p.exists() {
        return match read_file_limited(t, MAX_MD_BYTES) {
            Ok(text) => InitialDoc { name: short_name(t), text, base: parent_dir_of(t), source: t.to_string() },
            Err(e) => InitialDoc {
                name: "error".into(),
                text: format!("# Could not open file\n\n`{}`\n\n{}", t, e),
                base: String::new(),
                source: String::new(),
            },
        };
    }
    // Bare markdown passed on the command line.
    InitialDoc { name: "pasted".into(), text: t.to_string(), base: String::new(), source: String::new() }
}

fn read_path_as_doc(path: &PathBuf) -> Option<InitialDoc> {
    let s = path.to_string_lossy().to_string();
    read_file_limited(&s, MAX_MD_BYTES)
        .ok()
        .map(|text| InitialDoc { name: short_name(&s), text, base: parent_dir_of(&s), source: s.clone() })
}

/* ============ paths -> docs (drops / clipboard / file dialog) ============ */
// Every native entry point (drag-drop, CF_HDROP paste, file dialog) lands
// here: real filesystem paths in, docs-with-base out. The base is what makes
// relative images resolve — Rust serves them through http://mdview.localhost.

#[derive(serde::Serialize)]
struct DocPayload {
    text: String,
    name: String,
    base: String,
    /// Full path/URL for the internal database's source alias.
    source: String,
}

fn docs_from_paths(paths: &[PathBuf]) -> Vec<DocPayload> {
    let mut docs = Vec::new();
    for p in paths {
        if p.is_dir() {
            collect_md(p, 0, &mut docs);
        } else {
            push_doc(p, &mut docs);
        }
        if docs.len() >= MAX_DOC_BATCH {
            break;
        }
    }
    docs
}

fn push_doc(p: &Path, docs: &mut Vec<DocPayload>) {
    if docs.len() >= MAX_DOC_BATCH {
        return;
    }
    let s = p.to_string_lossy().to_string();
    if !is_doc_path(&s) {
        return;
    }
    if let Ok(text) = read_file_limited(&s, MAX_MD_BYTES) {
        docs.push(DocPayload { text, name: short_name(&s), base: parent_dir_of(&s), source: s.clone() });
    }
}

/// Markdown files in a dropped folder: top level, then one level down
/// (paper.md / sub/paper.md), capped — a huge tree must not become 50 tabs.
fn collect_md(dir: &Path, depth: u8, docs: &mut Vec<DocPayload>) {
    if docs.len() >= MAX_DOC_BATCH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut items: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    items.sort();
    for p in items {
        if docs.len() >= MAX_DOC_BATCH {
            return;
        }
        if p.is_dir() {
            if depth < 1 {
                collect_md(&p, depth + 1, docs);
            }
        } else {
            push_doc(&p, docs);
        }
    }
}

/// Read the text on the Windows clipboard directly, in the caller's process.
/// WebView2's navigator.clipboard.read() is permission-blocked here, so the
/// Paste button goes through Win32 like every other native read in the app.
fn clipboard_text() -> Result<String, String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13; // CF_UNICODETEXT
    const CF_TEXT: u32 = 1; // CF_TEXT
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("clipboard is busy".into());
        }
        let res = (|| -> Result<String, String> {
            const MAX_CHARS: usize = MAX_MD_BYTES / 2; // UTF-16 code units
            if IsClipboardFormatAvailable(CF_UNICODETEXT) != 0 {
                let h = GetClipboardData(CF_UNICODETEXT);
                if h.is_null() {
                    return Err("clipboard data unavailable".into());
                }
                let p = GlobalLock(h) as *const u16;
                if p.is_null() {
                    return Err("clipboard data unavailable".into());
                }
                // Bounded scan: a hostile or corrupt clipboard must not hang us.
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                    if len > MAX_CHARS {
                        let _ = GlobalUnlock(h);
                        return Err("clipboard text too large".into());
                    }
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
                let _ = GlobalUnlock(h);
                Ok(s)
            } else if IsClipboardFormatAvailable(CF_TEXT) != 0 {
                let h = GetClipboardData(CF_TEXT);
                if h.is_null() {
                    return Err("clipboard data unavailable".into());
                }
                let p = GlobalLock(h) as *const u8;
                if p.is_null() {
                    return Err("clipboard data unavailable".into());
                }
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                    if len > MAX_MD_BYTES {
                        let _ = GlobalUnlock(h);
                        return Err("clipboard text too large".into());
                    }
                }
                let s = String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned();
                let _ = GlobalUnlock(h);
                Ok(s)
            } else {
                Err("no text in the clipboard".into())
            }
        })();
        let _ = CloseClipboard();
        res
    }
}

/// Files copied in Explorer (CF_HDROP) -> their real paths.
fn clipboard_file_paths() -> Result<Vec<PathBuf>, String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows_sys::Win32::UI::Shell::DragQueryFileW;
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("clipboard is busy".into());
        }
        let res = (|| -> Result<Vec<PathBuf>, String> {
            let h = GetClipboardData(CF_HDROP_FMT);
            if h.is_null() {
                return Err("no file in the clipboard".into());
            }
            let count = DragQueryFileW(h, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
            let mut out = Vec::new();
            for i in 0..count {
                let len = DragQueryFileW(h, i, std::ptr::null_mut(), 0);
                let mut buf = vec![0u16; len as usize + 1];
                let got = DragQueryFileW(h, i, buf.as_mut_ptr(), buf.len() as u32);
                if got > 0 {
                    out.push(PathBuf::from(String::from_utf16_lossy(&buf[..got as usize])));
                }
                if out.len() >= MAX_DOC_BATCH {
                    break;
                }
            }
            if out.is_empty() {
                Err("no file in the clipboard".into())
            } else {
                Ok(out)
            }
        })();
        let _ = CloseClipboard();
        res
    }
}

fn clipboard_docs_json() -> Result<String, String> {
    let paths = clipboard_file_paths()?;
    let docs = docs_from_paths(&paths);
    if docs.is_empty() {
        return Err("no markdown file in the clipboard".into());
    }
    serde_json::to_string(&docs).map_err(|e| e.to_string())
}

/// Native open-file dialog (comdlg32) — the only picker that returns real
/// paths, so Browse gets a directory and images load like every other flow.
fn pick_file_dialog(owner: isize) -> Result<Vec<PathBuf>, String> {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetOpenFileNameW, OPENFILENAMEW, OFN_EXPLORER, OFN_FILEMUSTEXIST,
        OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
    };
    let mut buf = vec![0u16; 4096];
    let filter: Vec<u16> =
        "Markdown (*.md;*.markdown;*.txt)\0*.md;*.markdown;*.mdown;*.mkd;*.txt;*.text\0All files (*.*)\0*.*\0\0"
            .encode_utf16()
            .collect();
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner as HWND;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.Flags =
        OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_HIDEREADONLY;
    if unsafe { GetOpenFileNameW(&mut ofn) } != 0 {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..end]);
        if s.is_empty() {
            return Err("cancelled".into());
        }
        return Ok(vec![PathBuf::from(s)]);
    }
    let err = unsafe { CommDlgExtendedError() };
    if err == 0 {
        Err("cancelled".into())
    } else {
        Err(format!("file dialog error {err}"))
    }
}

fn short_name(p: &str) -> String {
    let s = p.replace('\\', "/");
    s.rsplit('/').next().unwrap_or("pasted").chars().take(40).collect()
}

fn build_html(initial: &InitialDoc) -> String {
    let template = include_str!("../frontend/index.html");
    let init_json = serde_json::to_string(&json!({ "name": initial.name, "text": initial.text, "base": initial.base, "source": initial.source }))
        .unwrap_or_else(|_| "{\"name\":\"\",\"text\":\"\",\"base\":\"\"}".into());
    template
        .replacen("__MDVIEW_INITIAL_JSON__", &init_json, 1)
        .replacen("__MDVIEW_PREFS_JSON__", &load_prefs_json(), 1)
        .replacen("__MDVIEW_HLJS_JS__", &hljs_js(), 1)
        .replacen("__MDVIEW_HLJS_LIGHT_CSS__", include_str!("../frontend/vendor/hljs-light.css"), 1)
        .replacen("__MDVIEW_HLJS_DARK_CSS__", &scope_css(include_str!("../frontend/vendor/hljs-dark.css"), "html.dark"), 1)
}

/// highlight.js is vendored (gzipped, ~43KB) and inlined: code highlighting
/// works fully offline with zero CDN dependence and zero load flash.
fn hljs_js() -> String {
    // `</script` inside the bundle would prematurely close our inline tag;
    // `<\/script` is identical semantics in both JS strings and regexes.
    String::from_utf8(gunzip(include_bytes!(
        "../frontend/vendor/hljs.min.js.gz"
    ))
    .expect("vendored hljs.min.js.gz must inflate"))
    .expect("hljs bundle is utf-8")
    .replace("</script", "<\\/script")
}

/// Prefix every rule's selectors with a scope (e.g. `html.dark`), so the
/// dark theme applies purely by class — no stylesheet toggling needed.
fn scope_css(css: &str, scope: &str) -> String {
    let mut out = String::with_capacity(css.len() + 256);
    for chunk in css.split('}') {
        if let Some(pos) = chunk.find('{') {
            let (sel, decl) = chunk.split_at(pos);
            let scoped: Vec<String> = sel
                .split(',')
                .map(|s| format!("{} {}", scope, s.trim()))
                .collect();
            out.push_str(&scoped.join(","));
            out.push_str(decl);
            out.push('}');
        } else {
            out.push_str(chunk);
        }
    }
    out
}

/* ================= preferences (Rust-owned file) ================= */
// NOTE: the view is loaded via with_html => opaque origin, where browsers
// refuse to persist localStorage (silently session-only). So prefs live in
// %APPDATA%\mdview\prefs.json, injected at boot, saved via "save_prefs" IPC.

fn prefs_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("mdview").join("prefs.json")
}

fn load_prefs_json() -> String {
    std::fs::read_to_string(prefs_path())
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "{\"theme\":null,\"fontScale\":1.0,\"readWidth\":860}".into())
}

fn save_prefs_json(text: &str) {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };
    let theme_in = v.get("theme").and_then(|t| t.as_str()).unwrap_or("light");
    let theme = if theme_in == "dark" { "dark" } else { "light" };
    let fs = v
        .get("fontScale")
        .and_then(|n| n.as_f64())
        .unwrap_or(1.0)
        .clamp(0.6, 2.2);
    let rw = v
        .get("readWidth")
        .and_then(|n| n.as_f64())
        .unwrap_or(860.0)
        .clamp(620.0, 1180.0) as i64;
    let tp_in = v.get("tocPos").and_then(|t| t.as_str()).unwrap_or("center");
    let tp = if tp_in == "left" || tp_in == "right" {
        tp_in
    } else {
        "center"
    };
    let out = json!({ "theme": theme, "fontScale": fs, "readWidth": rw, "tocPos": tp }).to_string();
    if let Some(dir) = prefs_path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(prefs_path(), out);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dump the real renderer output for a markdown file, so the README
    /// screenshots can be produced from the shipping render path instead of a
    /// re-implementation. Ignored by default:
    ///
    /// ```powershell
    /// cargo test --release fixture_render -- --ignored --nocapture
    /// ```
    ///
    /// Driven by `MDVIEW_FIXTURE_MD` (input) and `MDVIEW_FIXTURE_OUT`
    /// (rendered HTML fragment). See `tools/build-fixture.js`.
    #[test]
    #[ignore = "doc tooling, not a unit test"]
    fn fixture_render() {
        let (Ok(md_path), Ok(out)) = (
            std::env::var("MDVIEW_FIXTURE_MD"),
            std::env::var("MDVIEW_FIXTURE_OUT"),
        ) else {
            return;
        };
        let md = std::fs::read_to_string(&md_path).expect("fixture markdown must be readable");
        let base = std::path::Path::new(&md_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let html = render_markdown_to_html(&md, Some(&base));
        std::fs::write(&out, &html).expect("fixture output must be writable");
        eprintln!("fixture: {} -> {} ({} bytes)", md_path, out, html.len());
    }

    #[test]
    fn prefs_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mdview-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: tests run single-threaded here; APPDATA only affects this test.
        unsafe { std::env::set_var("APPDATA", &dir) };
        save_prefs_json("{\"theme\":\"dark\",\"fontScale\":1.5,\"readWidth\":1000,\"tocPos\":\"right\"}");
        let back = load_prefs_json();
        let v: serde_json::Value = serde_json::from_str(&back).unwrap();
        assert_eq!(v["theme"], "dark");
        assert!((v["fontScale"].as_f64().unwrap() - 1.5).abs() < 1e-9);
        assert_eq!(v["readWidth"], 1000);
        assert_eq!(v["tocPos"], "right");
        // Bogus tocPos sanitizes to center; other keys still persist.
        save_prefs_json("{\"theme\":\"dark\",\"fontScale\":1.5,\"readWidth\":1000,\"tocPos\":\"up\"}");
        let v2: serde_json::Value = serde_json::from_str(&load_prefs_json()).unwrap();
        assert_eq!(v2["tocPos"], "center");
        // Garbage in => safe light defaults, no panic.
        save_prefs_json("not json{{{");
        let back2: serde_json::Value = serde_json::from_str(&load_prefs_json()).unwrap();
        assert_eq!(back2["theme"], "dark"); // unchanged
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropped_paths_expand_to_docs() {
        let dir = std::env::temp_dir().join(format!("mdview-drop-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("sub"));
        std::fs::write(dir.join("one.md"), "# one").unwrap();
        std::fs::write(dir.join("sub").join("two.markdown"), "# two").unwrap();
        std::fs::write(dir.join("skip.rs"), "fn main(){}").unwrap();

        // A single file path opens with its directory as base.
        let docs = docs_from_paths(&[dir.join("one.md")]);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].name, "one.md");
        assert_eq!(docs[0].text, "# one");
        assert_eq!(docs[0].base, dir.to_string_lossy());

        // A dropped folder yields md at the top level and one level down,
        // while non-markdown files are skipped.
        let docs = docs_from_paths(&[dir.clone()]);
        let names: Vec<&str> = docs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"one.md"), "{names:?}");
        assert!(names.contains(&"two.markdown"), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with(".rs")), "{names:?}");

        // Dropping a non-document opens nothing.
        assert!(docs_from_paths(&[dir.join("skip.rs")]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_scan_caps_docs() {
        let dir = std::env::temp_dir().join(format!("mdview-cap-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        for i in 0..(MAX_DOC_BATCH + 4) {
            std::fs::write(dir.join(format!("f{i}.md")), "# x").unwrap();
        }
        let docs = docs_from_paths(&[dir.clone()]);
        assert!(!docs.is_empty());
        assert!(docs.len() <= MAX_DOC_BATCH, "{} docs", docs.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parent_relative_ref_is_normalized_not_rejected() {
        // `../images/x.png` is ubiquitous in papers. It must resolve to a
        // clean path the media handler will actually serve, not be rejected
        // as a traversal attempt.
        let root = std::env::temp_dir().join(format!("mdview-updot-{}", std::process::id()));
        let doc = root.join("doc");
        let figs = root.join("figures");
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::create_dir_all(&figs).unwrap();
        std::fs::write(figs.join("f.png"), b"x").unwrap();
        let got = resolve_media_ref("../figures/f.png", Some(&doc.to_string_lossy()));
        assert!(!got.contains(".."), "unnormalized: {got}");
        assert!(!got.contains("%2E%2E"), "encoded traversal: {got}");
        assert!(decode_local_media_url(&got).is_some(), "would be rejected: {got}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn figures_dir_may_sit_beside_the_paper_dir() {
        // Real layout: project/paper/paper.md + project/figures/fig1.png
        let root = std::env::temp_dir().join(format!("mdview-figs-{}", std::process::id()));
        let paper = root.join("paper");
        let figs = root.join("figures");
        std::fs::create_dir_all(&paper).unwrap();
        std::fs::create_dir_all(&figs).unwrap();
        std::fs::write(figs.join("fig1.png"), b"x").unwrap();
        let base = paper.to_string_lossy().to_string();
        // paper/figures/fig1.png is missing, ../figures/fig1.png exists -> use it.
        let got = resolve_media_ref("figures/fig1.png", Some(&base));
        assert!(
            got.contains("mdview.localhost/asset") || got.contains("mdview.localhost/local"),
            "unexpected scheme: {got}"
        );
        assert!(got.to_lowercase().contains("figures/fig1.png"), "{got}");
        // It must point at the real file (the parent one), not paper/figures.
        assert!(!got.contains("paper/figures"), "{got}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn exact_relative_ref_is_never_rewritten() {
        // A correct document must resolve to exactly base/rel, even though a
        // parent directory also has a file of that name.
        let root = std::env::temp_dir().join(format!("mdview-exact-{}", std::process::id()));
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("x.png"), b"a").unwrap();
        std::fs::write(b.join("x.png"), b"b").unwrap();
        let got = resolve_media_ref("x.png", Some(&a.to_string_lossy()));
        assert!(!got.contains("/b/"), "{got}");
        assert!(got.to_lowercase().contains("/a/x.png"), "{got}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn vendored_gzip_blobs_inflate() {
        // Guards the build steps that gzip vendor files (tools/vendor-assets.py,
        // tools/vendor-cm6.mjs): a corrupt or truncated blob must fail here, loudly,
        // rather than silently at runtime as a blank page.
        for (name, gz) in [
            (
                "hljs",
                &include_bytes!("../frontend/vendor/hljs.min.js.gz")[..],
            ),
            (
                "katex.js",
                &include_bytes!("../frontend/vendor/katex/katex.min.js.gz")[..],
            ),
            (
                "katex.css",
                &include_bytes!("../frontend/vendor/katex/katex.min.css.gz")[..],
            ),
            (
                "mermaid",
                &include_bytes!("../frontend/vendor/mermaid/mermaid.min.js.gz")[..],
            ),
            (
                "codemirror",
                &include_bytes!("../frontend/vendor/codemirror.min.js.gz")[..],
            ),
        ] {
            let out = gunzip(gz).unwrap_or_else(|| panic!("{name} failed to inflate"));
            assert!(out.len() > 1000, "{name} inflated to only {} bytes", out.len());
            // Inflated payload must be real text, not compressed noise. Minified
            // bundles differ in how they open (comment banner, IIFE, ...), so
            // look for a marker that must appear somewhere in each one.
            let text = String::from_utf8_lossy(&out);
            let marker = match name {
                "hljs" => "highlight.js",
                "katex.css" => "KaTeX",
                "katex.js" => "katex",
                "mermaid" => "mermaid",
                "codemirror" => "MDViewCM",
                other => panic!("unknown blob {other}"),
            };
            assert!(text.contains(marker), "{name} lacks {marker} marker");
        }
        // garbage must be rejected, not panic
        assert!(gunzip(b"not gzip at all").is_none());
        assert!(gunzip(&[0x1f, 0x8b, 0, 0]).is_none()); // truncated header
    }

    #[test]
    fn tables_are_wrapped_in_a_scroll_container() {
        // The wrapper is what lets a wide table scroll without the page doing
        // so, and what gives the sticky header a scrollport to pin against.
        let html = wrap_tables("<p>a</p><table><thead><tr><th>H</th></tr></thead></table>".into());
        assert!(html.contains("<div class=\"tw\"><table>"), "{}", html);
        assert!(html.contains("</table></div>"), "{}", html);
        assert_eq!(html.matches("<div class=\"tw\">").count(), 1);
    }

    #[test]
    fn wrapping_preserves_text_and_ignores_tag_prefixes() {
        // Byte scanning must not mangle multi-byte text, and must not treat a
        // tag that merely starts with "table" as a table.
        let html = wrap_tables("<p>caf\u{e9} \u{4f60}\u{597d}</p><tablet>x</tablet>".into());
        assert!(html.contains("caf\u{e9} \u{4f60}\u{597d}"), "{}", html);
        assert!(html.contains("<tablet>"), "{}", html);
        assert!(!html.contains("<div class=\"tw\"><tablet>"), "{}", html);
    }

    #[test]
    fn nested_tables_close_their_own_wrappers() {
        // A depth counter, not a blind find/replace: the inner </table> must
        // not close the outer wrapper early.
        let html = wrap_tables(
            "<table><tr><td><table><tr><td>x</td></tr></table></td></tr></table>".into(),
        );
        assert_eq!(html.matches("<div class=\"tw\">").count(), 2, "{}", html);
        assert_eq!(html.matches("</table></div>").count(), 2, "{}", html);
        assert!(html.ends_with("</table></div>"), "{}", html);
    }

    #[test]
    fn rendered_tables_carry_the_wrapper() {
        let out = render_markdown_to_html("| a | b |\n| --- | --- |\n| 1 | 2 |\n", None);
        assert!(out.contains("class=\"tw\""), "{}", out);
    }

    #[test]
    fn app_assets_are_served_offline() {
        // The whole point of vendoring: these resolve with zero network.
        for (path, needle) in [
            ("katex/katex.min.js", "katex"),
            ("mermaid/mermaid.min.js", "mermaid"),
            ("codemirror/codemirror.min.js", "CodeMirror"),
        ] {
            let uri = format!("http://mdview.localhost/asset/{path}");
            let r = serve_app_asset(&uri)
                .unwrap_or_else(|| panic!("{path} not served"));
            assert_eq!(r.status(), 200);
            assert!(r.headers()["Content-Type"].to_str().unwrap_or("").starts_with("text/javascript"), "{path}");
            assert!(r.body().len() > 1000, "{path} too small: {}", r.body().len());
            assert!(
                String::from_utf8_lossy(&r.body()).contains(needle),
                "{path} body lacks {needle}"
            );
        }
        // CSS must be served with absolute font URLs (page has an opaque origin).
        let css = serve_app_asset("http://mdview.localhost/asset/katex/katex.min.css").unwrap();
        let body = String::from_utf8_lossy(css.body());
        assert!(body.contains("http://mdview.localhost/asset/katex/fonts/"), "fonts not absolutized");
        assert!(!body.contains("url(fonts/"), "relative font URL survived");
        // A real font resolves, and a bogus one does not.
        let f = serve_app_asset("http://mdview.localhost/asset/katex/fonts/KaTeX_Main-Regular.woff2").unwrap();
        assert_eq!(f.headers()["Content-Type"], "font/woff2");
        assert!(f.body().len() > 1000);
        assert!(serve_app_asset("http://mdview.localhost/asset/katex/fonts/nope.woff2").is_none());
        // Unknown assets stay unknown (fall through to the media handler).
        assert!(serve_app_asset("http://mdview.localhost/asset/nope.js").is_none());
        // The inflated cache can be dropped back to empty at any time.
        release_asset_cache();
    }

    #[test]
    fn render_strips_scripts() {
        let html = render_markdown_to_html("# hi\n\n<script>alert(1)</script>\n\n[text](javascript:alert(1))", None);
        assert!(!html.to_lowercase().contains("<script"));
        assert!(!html.contains("javascript:"));
        assert!(html.contains("hi"));
    }

    #[test]
    fn media_urls_resolve_against_http_base() {
        let base = "https://raw.githubusercontent.com/u/r/refs/heads/main/README.md";
        let html = render_markdown_to_html("![a](./demo.gif)\n\n![b](../x/y.png)\n\n![c](/root.png)\n\n![d](https://x.io/i.png)\n\n<video src=\"clip.mp4\"></video>", Some(base));
        assert!(html.contains("https://raw.githubusercontent.com/u/r/refs/heads/main/demo.gif"), "{}", html);
        assert!(html.contains("https://raw.githubusercontent.com/u/r/refs/heads/x/y.png"), "{}", html);
        assert!(html.contains("https://raw.githubusercontent.com/root.png"), "{}", html);
        assert!(html.contains("https://x.io/i.png"), "{}", html);
        assert!(html.contains("https://raw.githubusercontent.com/u/r/refs/heads/main/clip.mp4"), "{}", html);
        assert!(html.contains("<video controls"), "{}", html);
    }

    #[test]
    fn media_urls_resolve_against_file_base() {
        let html = render_markdown_to_html("![a](img\\a.png)\n\n![b](C:\\pics\\b.jpg)\n\n![c](data:image/png;base64,xx)", Some("C:\\docs"));
        // WebView2-fetchable workaround host, not the bare custom scheme.
        assert!(html.contains("http://mdview.localhost/local/C:/docs/img/a.png"), "{}", html);
        assert!(html.contains("http://mdview.localhost/local/C:/pics/b.jpg"), "{}", html);
        assert!(html.contains("data:image/png;base64,xx"), "{}", html);
    }

    #[test]
    fn media_urls_keep_anchors_and_bare() {
        let html = render_markdown_to_html("[t](#sec)\n\n![a](./x.png)", None);
        assert!(html.contains("#sec"), "{}", html);
        assert!(html.contains("./x.png"), "{}", html); // no base: untouched
    }

    #[test]
    fn remote_img_src_rewrites_to_fetch_handler() {
        let html = render_markdown_to_html("![d](https://x.io/i.png)", None);
        assert!(html.contains("src=\"http://mdview.localhost/fetch?url=https://x.io/i.png\""), "{}", html);
        assert!(!html.contains("src=\"https://x.io/i.png\""), "{}", html);
    }

    #[test]
    fn remote_fetch_url_roundtrips() {
        let url = "https://x.io/serve?domain=https://a.b/c&sz=32#frag";
        assert_eq!(decode_fetch_url(&to_fetch_url(url)).as_deref(), Some(url));
    }

    #[test]
    fn mdview_serve_urls_are_not_double_wrapped() {
        assert!(is_mdview_serve("http://mdview.localhost/fetch?url=x"));
        assert!(is_mdview_serve("mdview.localhost/local/C:/a.png"));
        assert!(!is_mdview_serve("https://mdview.localhost.evil/x.png"));
        let html = render_markdown_to_html("![a](http://mdview.localhost/local/C:/pics/b.jpg)", None);
        assert!(html.contains("src=\"http://mdview.localhost/local/C:/pics/b.jpg\""), "{}", html);
    }

    #[test]
    fn link_to_image_extension_upgrades_to_img() {
        let html = render_markdown_to_html("[pic](https://x.io/i.png)", None);
        assert!(html.contains("<img src=\"http://mdview.localhost/fetch?url=https://x.io/i.png\""), "{}", html);
        assert!(!html.contains("<a href=\"https://x.io/i.png\""), "{}", html);
    }

    #[test]
    fn link_to_known_non_image_stays_a_link() {
        let html = render_markdown_to_html("[doc](https://x.io/f.pdf)", None);
        assert!(html.contains("<a href=\"https://x.io/f.pdf\""), "{}", html);
        assert!(!html.contains("<img"), "{}", html);
    }

    #[test]
    fn ambiguous_link_stays_a_link_without_probe() {
        // Probe disabled: no network, no upgrade, never guess.
        LINK_PROBE_NETWORK.store(false, Ordering::Relaxed);
        let html = render_markdown_to_html("[maybe](https://x.io/serve?domain=https://a.b&sz=32)", None);
        LINK_PROBE_NETWORK.store(true, Ordering::Relaxed);
        assert!(html.contains("<a href=\"https://x.io/serve?domain=https://a.b&amp;sz=32\""), "{}", html);
        assert!(!html.contains("<img"), "{}", html);
    }

    #[test]
    fn link_wrapper_around_img_is_not_upgraded() {
        // A link wrapping an image is deliberate markup (media anchors).
        let md = "[![thumb](https://x.io/t.png)](https://x.io/clip)";
        let html = render_markdown_to_html(md, None);
        assert!(html.contains("<a href=\"https://x.io/clip\""), "{}", html);
        assert!(html.contains("<img"), "{}", html); // the inner image stays an image
    }

    #[test]
    fn probe_head_helper_respects_flag() {
        LINK_PROBE_NETWORK.store(false, Ordering::Relaxed);
        assert_eq!(probe_remote_link("https://x.io/anything"), None);
        LINK_PROBE_NETWORK.store(true, Ordering::Relaxed);
    }

    // End-to-end against the real network: excluded from the default suite so
    // CI never depends on connectivity. Run explicitly with:
    //   cargo test --bin mdview remote_image_e2e -- --ignored --nocapture
    #[test]
    #[ignore]
    fn remote_image_e2e() {
        let favicon = "https://www.google.com/s2/favicons?domain=https://www.nice.org.uk&sz=32";
        let (ctype, bytes) = fetch_remote_image(favicon).expect("extensionless favicon must fetch");
        assert!(ctype.starts_with("image/"), "{ctype}");
        assert!(!bytes.is_empty());
        println!("favicon: {ctype}, {} bytes", bytes.len());

        let raw = "https://raw.githubusercontent.com/DiyarD/mdview/refs/heads/master/docs/screenshots/01-reading-light.png";
        let (ctype, bytes) = fetch_remote_image(raw).expect("raw github image must fetch");
        assert!(ctype.starts_with("image/"), "{ctype}");
        assert!(bytes.len() > 1024, "expected a real PNG, got {} bytes", bytes.len());
        println!("raw github png: {ctype}, {} bytes", bytes.len());
    }

    #[test]
    #[ignore]
    fn remote_link_to_image_upgrades_e2e() {
        // The user's exact case: a markdown link whose target is an image,
        // with no image extension.
        let md = "[image](https://www.google.com/s2/favicons?domain=https://www.nice.org.uk&sz=32)";
        let html = render_markdown_to_html(md, None);
        println!("{html}");
        assert!(html.contains("<img src=\"http://mdview.localhost/fetch?url="), "{html}");
    }

    #[test]
    fn multibyte_content_survives_pipeline() {
        // Regression: byte-indexed scanners panicked on non-ASCII input.
        let md = "# سلاو 🌍\n\nکوردی unicode تێست 🎬\n\n![w](./img/وێنە.png)\n\n<SCRIPT>alert(1)</SCRIPT>\n\n<video src=\"v.mp4\"></video>";
        let html = render_markdown_to_html(md, Some("https://x.io/u/r/main/README.md"));
        assert!(html.contains("سلاو"));
        assert!(html.contains("🌍"));
        assert!(!html.to_lowercase().contains("<script"));
        assert!(html.contains("https://x.io/u/r/main/img/"), "{}", html);
        assert!(html.contains("<video controls"));
    }

    /* ================= block spans (editor mapping) ================= */

    const FULL_FIXTURE: &str = "---\ntitle: Test & \"front matter\"\n---\n\nSetext Heading\n======\n\n# ATX heading\n\nпараagraph with **bold** and $x^2$ math\n\n- item a\n  - nested b\n- item c\n\n> quoted text\n> more quote\n\n```rust\n# not a heading\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nFootnote text[^1]\n\n[^1]: the note\n\n<div class=\"x\">raw</div>\n\n***\n";

    #[test]
    fn block_spans_cover_every_block_type_in_order() {
        let spans = block_spans(FULL_FIXTURE);
        let got: Vec<&str> = spans.iter().map(|s| s.kind).collect();
        assert_eq!(
            got,
            ["frontmatter", "heading", "heading", "paragraph", "list", "quote", "code", "table", "paragraph", "footnote", "html", "rule"],
            "{:?}",
            spans
        );
        // Indices are dense and spans are ordered and non-overlapping.
        for (i, s) in spans.iter().enumerate() {
            assert_eq!(s.index, i);
        }
        for w in spans.windows(2) {
            assert!(w[0].end_line <= w[1].start_line, "overlapping: {:?} vs {:?}", w[0], w[1]);
        }
        // Every span's first line begins a block; no span starts on a blank line.
        let lines: Vec<&str> = FULL_FIXTURE.split('\n').collect();
        for s in &spans {
            assert!(!lines[s.start_line].trim().is_empty(), "span starts on blank line: {:?}", s);
        }
        // Spot-check exact extents of known blocks.
        let find = |k: &str, n: usize| spans.iter().filter(|s| s.kind == k).nth(n).unwrap();
        assert_eq!((find("frontmatter", 0).start_line, find("frontmatter", 0).end_line), (0, 3));
        assert_eq!((find("heading", 0).start_line, find("heading", 0).end_line), (4, 6)); // setext: text + underline
        assert_eq!((find("heading", 1).start_line, find("heading", 1).end_line), (7, 8));
        assert_eq!((find("list", 0).start_line, find("list", 0).end_line), (11, 14));
        assert_eq!((find("code", 0).start_line, find("code", 0).end_line), (18, 21));
        assert_eq!((find("footnote", 0).start_line, find("footnote", 0).end_line), (28, 29));
        assert_eq!((find("rule", 0).start_line, find("rule", 0).end_line), (32, 33));
    }

    #[test]
    fn block_spans_use_line_coordinates_not_byte_offsets() {
        // Emoji and CJK must not shift the reported lines: a byte-counted
        // mapping would drift by one line per astral character here.
        let md = "# 🌍🌍🌍\n\n中文段落 with 🎵🎵🎵\n\n- 项目\n";
        let spans = block_spans(md);
        let kinds: Vec<&str> = spans.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, ["heading", "paragraph", "list"]);
        assert_eq!((spans[0].start_line, spans[0].end_line), (0, 1));
        assert_eq!((spans[1].start_line, spans[1].end_line), (2, 3));
        assert_eq!((spans[2].start_line, spans[2].end_line), (4, 5));
    }

    #[test]
    fn block_spans_handle_crlf_documents() {
        let md = "# hi\r\n\r\npara one\r\n\r\n- a\r\n- b\r\n";
        let spans = block_spans(md);
        let kinds: Vec<&str> = spans.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, ["heading", "paragraph", "list"]);
        assert_eq!((spans[0].start_line, spans[0].end_line), (0, 1));
        assert_eq!((spans[1].start_line, spans[1].end_line), (2, 3));
        assert_eq!((spans[2].start_line, spans[2].end_line), (4, 6));
    }

    #[test]
    fn block_spans_fenced_code_with_markdown_looking_content() {
        let md = "```\n# not a heading\n\n- not a list\n```\n\nreal paragraph\n";
        let spans = block_spans(md);
        assert_eq!(spans.iter().map(|s| s.kind).collect::<Vec<_>>(), ["code", "paragraph"]);
        assert_eq!((spans[0].start_line, spans[0].end_line), (0, 5));
        assert_eq!((spans[1].start_line, spans[1].end_line), (6, 7));
    }

    #[test]
    fn block_spans_mermaid_and_math_blocks() {
        let md = "```mermaid\ngraph TD\n  A-->B\n```\n\n$$\nx = 1\n$$\n\ntext $y = 2$ after\n";
        let spans = block_spans(md);
        // Mermaid fence is a code block; pulldown emits display math as a
        // paragraph wrapping an inline DisplayMath leaf — so the math paragraph
        // IS the editable block (raw $$...$$ source visible in the cell).
        let kinds: Vec<&str> = spans.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, ["code", "paragraph", "paragraph"], "{:?}", spans);
        assert_eq!((spans[1].start_line, spans[1].end_line), (5, 8));
        assert_eq!((spans[2].start_line, spans[2].end_line), (9, 10));
    }

    #[test]
    fn script_only_block_renders_as_empty_wrapper_not_nothing() {
        // Every markdown block must produce a .md-block wrapper or the editor
        // mapping drifts for all subsequent blocks.
        let md = "# hi\n\n<script>alert(1)</script>\n\npara\n";
        let html = render_markdown_to_html(md, None);
        assert_eq!(html.matches("<div class=\"md-block\"").count(), 3, "{html}");
        assert!(!html.to_lowercase().contains("<script"), "{html}");
        let spans = block_spans(md);
        assert_eq!(spans.iter().map(|s| s.kind).collect::<Vec<_>>(), ["heading", "html", "paragraph"]);
    }

    #[test]
    fn rendered_wrappers_are_dense_and_match_span_count() {
        for doc in [
            FULL_FIXTURE,
            "# a\n\npara\n",
            "<script>x</script>\n",
            "---\nyaml: true\n---\n",
            "***\n",
            "",
        ] {
            let html = render_markdown_to_html(doc, None);
            let spans = block_spans(doc);
            let wrappers = html.matches("<div class=\"md-block\"").count();
            assert_eq!(wrappers, spans.len(), "doc {:?}: html {} vs spans {:?}", doc, wrappers, spans);
            for i in 0..wrappers {
                assert!(html.contains(&format!("data-blk=\"{}\"", i)), "missing data-blk {i} in {html}");
            }
        }
    }

    #[test]
    fn tables_render_inside_their_block_wrapper() {
        let html = render_markdown_to_html("| a | b |\n|--- | --- |\n| 1 | 2 |\n", None);
        let wrapper_at = html.find("<div class=\"md-block\"").expect("wrapper");
        let tw_at = html.find("<div class=\"tw\">").expect("tw wrapper");
        assert!(wrapper_at < tw_at, "md-block must enclose the table's .tw: {html}");
        assert!(html.matches("<div class=\"md-block\"").count() == 1, "{html}");
    }

    #[test]
    fn splicing_block_by_line_extents_round_trips() {
        // The exact operation the editor performs: replace the characters of
        // one block's lines with new source, leave everything else byte-identical.
        for doc in [FULL_FIXTURE.to_string(), "# 🌍 x\n\n中文 para\n".to_string(), "a\n\nb\n\nc\n".to_string()] {
            let spans = block_spans(&doc);
            // Build line start char offsets (char-based, like the JS side).
            let mut line_starts = vec![0usize];
            for (i, ch) in doc.char_indices() {
                if ch == '\n' { line_starts.push(i + 1); }
            }
            let end_of_doc = doc.char_indices().map(|(i, _)| i).last().map(|i| i + 1).unwrap_or(0);
            let char_end = end_of_doc;
            let block_text = |s: &BlockSpan| -> String {
                let st = line_starts[s.start_line];
                let en = if s.end_line < line_starts.len() { line_starts[s.end_line] } else { char_end };
                doc[st..en].to_string()
            };
            for (i, s) in spans.iter().enumerate() {
                let block = block_text(s);
                assert!(doc.contains(&block), "block {i} slice not found");
                let edited = format!("EDITED {}\n", i);
                let new_doc = doc.replacen(&block, &edited, 1);
                assert!(new_doc.contains(&edited), "splice broke block {i}");
            }
        }
    }

    #[test]
    fn hljs_vendoring_is_safe() {
        // Inline <script> must never contain a literal closing tag...
        assert!(!hljs_js().contains("</script"));
        assert!(hljs_js().contains("highlightAuto"));
        // ...and dark rules must be scoped so light is the default.
        let scoped = scope_css(".hljs{color:#fff}.hljs-kw,.hljs-str{color:#000}", "html.dark");
        assert_eq!(scoped, "html.dark .hljs{color:#fff}html.dark .hljs-kw,html.dark .hljs-str{color:#000}");
    }

    #[test]
    fn video_controls_not_duplicated() {
        let out = ensure_video_controls("<video controls src=\"a.mp4\"></video>".to_string());
        assert_eq!(out.matches("controls").count(), 1, "{}", out);
    }

    #[test]
    fn local_media_serving_gates() {
        // Rejects: traversal, bad extension, missing file.
        let r = serve_local_media("mdview://local/C:/x/../evil.png");
        assert_eq!(r.status(), http::StatusCode::NOT_FOUND);
        let r = serve_local_media("mdview://local/C:/x/tool.exe");
        assert_eq!(r.status(), http::StatusCode::NOT_FOUND);
        let r = serve_local_media("mdview://local/C:/definitely/not/here.png");
        assert_eq!(r.status(), http::StatusCode::NOT_FOUND);
        // Serves a real temp png with the right type.
        let dir = std::env::temp_dir().join(format!("mdview-media-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("a.png");
        std::fs::write(&p, vec![137u8, 80, 78, 71]).unwrap();
        let uri = to_local_url(&p.to_string_lossy());
        // URL must be the WebView2-fetchable http form...
        assert!(uri.starts_with("http://mdview.localhost/local/"), "{uri}");
        let r = serve_local_media(&uri);
        assert_eq!(r.status(), http::StatusCode::OK);
        assert_eq!(r.headers()["Content-Type"], "image/png");
        assert_eq!(r.body().as_ref(), &[137u8, 80, 78, 71]);
        // ...and the handler must accept every form that can reach it:
        // wry reverts http://mdview.localhost/... to mdview://localhost/...,
        // and legacy links may still carry bare mdview://local/....
        for probe in [
            uri.replace("http://mdview.localhost", "mdview://localhost"),
            uri.replace("http://mdview.localhost", "mdview://"),
        ] {
            let r = serve_local_media(&probe);
            assert_eq!(r.status(), http::StatusCode::OK, "{probe}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        // Percent roundtrip incl. spaces.
        assert_eq!(percent_decode(&percent_encode_path("C:\\my dir\\a b.png")), "C:\\my dir\\a b.png");
    }
}

/* ================= single instance (loopback TCP, std only) ================= */

fn signal_primary(open_arg: &Option<String>) {
    let msg = match open_arg {
        Some(a) if !a.starts_with("--") => format!("open-file {}\n", a),
        _ => "new-tab\n".to_string(),
    };
    if let Ok(addr) = SINGLE_ADDR.parse() {
        if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(800)) {
            let _ = std::io::Write::write_all(&mut s, msg.as_bytes());
        }
    }
}

fn spawn_accept_loop(listener: TcpListener, proxy: tao::event_loop::EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = vec![0u8; 4096];
            let Ok(n) = std::io::Read::read(&mut s, &mut buf) else { continue };
            let msg = String::from_utf8_lossy(&buf[..n]).to_string();
            if let Some(path) = msg.strip_prefix("open-file") {
                let path = path.trim().to_string();
                if path.is_empty() {
                    let _ = proxy.send_event(UserEvent::NewTabRemote);
                } else if path.starts_with("http://") || path.starts_with("https://") {
                    match fetch_url_text(&path) {
                        Ok(text) => {
                            let _ = proxy.send_event(UserEvent::OpenTabRemote {
                                name: short_name(&path),
                                text,
                                base: path.clone(),
                                source: path.clone(),
                            });
                        }
                        Err(_) => {
                            let _ = proxy.send_event(UserEvent::NewTabRemote);
                        }
                    }
                } else {
                    match read_file_limited(&path, MAX_MD_BYTES) {
                        Ok(text) => {
                            let _ = proxy.send_event(UserEvent::OpenTabRemote {
                                name: short_name(&path),
                                text,
                                base: parent_dir_of(&path),
                                source: path.clone(),
                            });
                        }
                        Err(_) => {
                            let _ = proxy.send_event(UserEvent::NewTabRemote);
                        }
                    }
                }
            } else {
                let _ = proxy.send_event(UserEvent::NewTabRemote);
            }
        }
    });
}

/* ================= global hotkey via tiny tray helper ================= */
// The old Start-Menu .lnk Hotkey trick is unreliable (Explorer often ignores
// new shortcuts, combos get stolen silently). Instead a ~1MB tray process
// owns a REAL RegisterHotKey hotkey and launches us. Still ~zero cost:
// no webview, no runtime, blocking waits only.

fn tray_exe_path() -> Option<PathBuf> {
    let cur = std::env::current_exe().ok()?;
    let dir = cur.parent()?;
    #[cfg(target_os = "windows")]
    let cand = dir.join("mdview-tray.exe");
    #[cfg(not(target_os = "windows"))]
    let cand = dir.join("mdview-tray");
    Some(cand)
}

fn startup_link() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
        .join("mdview-tray.lnk")
}

/// Remove the legacy .lnk-hotkey shortcuts (they never fired reliably).
fn remove_legacy_links() {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let start = PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("mdview.lnk");
    let _ = std::fs::remove_file(&start);
    if let Ok(home) = std::env::var("USERPROFILE") {
        let _ = std::fs::remove_file(PathBuf::from(home).join("Desktop").join("mdview.lnk"));
    }
}

fn install_hotkey() -> i32 {
    let Some(tray) = tray_exe_path() else {
        eprintln!("cannot locate exe dir");
        return 1;
    };
    if !tray.exists() {
        eprintln!("tray helper not found at {}", tray.display());
        eprintln!("build it with: cargo build --release");
        return 1;
    }
    remove_legacy_links();
    let link = startup_link();
    let ps = format!(
        "$ws = New-Object -ComObject WScript.Shell; \
         $s = $ws.CreateShortcut('{}'); \
         $s.TargetPath = '{}'; \
         $s.WorkingDirectory = '{}'; \
         $s.Description = 'mdview tray helper (global hotkey)'; \
         $s.Save();",
        link.display().to_string().replace('\'', "''"),
        tray.display().to_string().replace('\'', "''"),
        tray.parent().map(|p| p.display().to_string()).unwrap_or_default().replace('\'', "''"),
    );
    let ok = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .status()
        .map(|st| st.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("failed to create autostart shortcut");
        return 1;
    }
    // (Re)launch the helper now (it self-dedups if already running).
    let _ = std::process::Command::new(&tray).spawn();
    println!("Installed tray helper (autostart: {})", link.display());
    println!("Press Ctrl+Shift+M anywhere (tray picks a free combo if taken).");
    println!("Right-click the tray icon to open mdview or quit the helper.");
    0
}

fn uninstall_hotkey() -> i32 {
    remove_legacy_links();
    let link = startup_link();
    match std::fs::remove_file(&link) {
        Ok(_) => println!("Removed {}", link.display()),
        Err(e) => eprintln!("autostart link: {} ({e})", link.display()),
    }
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "mdview-tray.exe"])
        .status();
    0
}
