# mdview

**A markdown viewer you open by pressing a key.**

No Electron. No Node. No async runtime. One native window on the WebView2 engine
that already ships with Windows, plus a 0.4 MB tray helper that owns the global
hotkey. Markdown is parsed in Rust, so the renderer libraries never touch the
network — math, diagrams and syntax highlighting all work with the cable
unplugged.

![mdview reading a document](docs/screenshots/01-reading-light.png)

---

## Contents

- [Why it exists](#why-it-exists)
- [Getting markdown in](#getting-markdown-in)
- [Rendering](#rendering)
  - [Code](#code)
  - [Math](#math)
  - [Diagrams](#diagrams)
  - [Tables](#tables)
  - [Images, video and audio](#images-video-and-audio)
- [Reading comfortably](#reading-comfortably)
  - [The contents pill](#the-contents-pill)
  - [Theme, width and text size](#theme-width-and-text-size)
- [Printing and PDF](#printing-and-pdf)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [Command line](#command-line)
- [Why it uses almost no memory](#why-it-uses-almost-no-memory)
- [Install](#install)
- [Build from source](#build-from-source)
- [How it works](#how-it-works)
- [Limits and safety](#limits-and-safety)
- [Known rough edges](#known-rough-edges)
- [Development](#development)

---

## Why it exists

Most markdown viewers make you go and find a file. mdview inverts that: you press
**Ctrl+Shift+M** anywhere in Windows and a window is already open and waiting for
something to paste.

That one decision drives the rest of the design. If the common case is a
clipboard, then:

- the window has to appear *instantly* and be cheap to keep around,
- it has to work with whatever is on the clipboard, not just text,
- and it has to survive a 4 MB paste without falling over.

![The whole document, top to bottom](docs/screenshots/02-full-document.png)

*Every feature in one document: prose, task lists, footnotes, four languages of
code, maths, a diagram, two tables and images — all of it rendered locally, with
no network involved. This is [`docs/demo/field-notes.md`](docs/demo/field-notes.md),
and every screenshot below is taken from it.*

---

## Getting markdown in

Press the hotkey, then <kbd>Ctrl</kbd>+<kbd>V</kbd>. mdview works out what you
handed it:

| You pasted | What happens |
|---|---|
| **Markdown or plain text** | Rendered as GitHub-flavoured markdown. |
| **A file copied in Explorer** | The real path is read off the clipboard (`CF_HDROP`), so relative images resolve against *that file's* folder. |
| **A file path** | `C:\notes\paper.md`, `.\paper.md` or a UNC path is read from disk. |
| **A URL** | Fetched and rendered, with `./demo.gif` resolving against the remote page — so a GitHub README renders its own images. |

Anything else works too:

- **Drag & drop** files or folders anywhere in the window. Dropping a folder
  opens every markdown file inside it, one level deep.
- **Browse files…** opens the native file picker.
- **Launch it with a file** — `mdview.exe notes.md` from anywhere, including
  Explorer's address bar or a shortcut. A second launch never opens a second
  window; it hands the file to the window you already have.
- **Right-click the tray icon** to open mdview, or to quit the helper.

Recognised document extensions: `.md`, `.markdown`, `.mdown`, `.mkd`, `.txt`,
`.text`. One drop or paste opens up to 8 documents, each in its own tab.

---

## Rendering

Markdown is rendered by [`pulldown-cmark`](https://github.com/pulldown-cmark/pulldown-cmark)
in Rust with tables, footnotes, strikethrough, task lists, heading attributes,
YAML front matter and math all enabled — then sanitised and post-processed.
Every renderer library is vendored, gzipped into the executable and inflated on
demand, so **there is no CDN and no network dependency anywhere**.

### Code

Fenced blocks are highlighted by a vendored [highlight.js](https://highlightjs.org)
that is inflated out of binary at boot. An **unlabelled fence is auto-detected**
across a 20-language subset, so a stray ``` block usually still comes out right.

![Code blocks, syntax highlighted](docs/screenshots/04-code.png)

The palette is hand-tuned rather than vendored — muted editorial colours that sit
with the warm paper theme instead of fighting it. `plaintext`, `text`,
`nohighlight` and `none` opt a block out of highlighting entirely.

### Math

Math is **parsed in Rust** and typeset by a vendored [KaTeX](https://katex.org).
Parsing first matters: it means GFM emphasis can never mangle something like
`$s_{it}$`, and delimiters are consumed exactly once.

Inline math sits in the line with everything else; display blocks get their own
centred measure and scroll sideways rather than widening the column.

![KaTeX-rendered inline and display math](docs/screenshots/05-math.png)

### Diagrams

A ` ```mermaid ` fence becomes a real diagram, themed to whichever side of the
light/dark pair is active.

![A Mermaid diagram in the light theme](docs/screenshots/06-diagram.png)

Mermaid is the one fiddly corner: node labels are HTML inside a `foreignObject`,
so a `classDef … fill:` never reaches the text and labels end up light-on-light in
dark mode. Rather than fight it, mdview reads the fill the diagram **actually
painted**, computes its WCAG relative luminance, and picks the label ink from
that.

![The same diagram in dark mode, labels still legible](docs/screenshots/10-dark-diagram.png)

### Tables

Tables are where viewers usually fall apart, so this is where most of the CSS
effort went:

- every table is wrapped in its own **scroll container** with a **sticky header**
  that pins to the frame rather than to the page,
- cells are capped at a comfortable measure, so a long cell **wraps** instead of
  stretching the page sideways,
- `max-width` on a *cell* is what actually kills runaway tables, while keeping each
  column at its natural relative width — a narrow "Scope" column stays narrow,
- numeric columns get tabular figures, rows zebra-stripe and highlight on hover,
- a table wider than the reading column degrades to a **full-width frame with its
  own horizontal scrollbar**, rather than making the whole page scroll.

![Tables with sticky headers, wrapped cells and zebra rows](docs/screenshots/07-tables.png)

### Images, video and audio

Relative references resolve against the **document**, not the working directory —
the folder you opened the file from, or the URL you fetched it from. So a
markdown file and its `assets/` folder travel together, and a GitHub README
renders its own images and `<video>` blocks.

![A relative image reference resolving against the document](docs/screenshots/08-media.png)

Local files are streamed through an internal protocol with a **100 MB ceiling**,
served only for a fixed allowlist of image, video and audio extensions. Markdown
files are never served through it — they are opened as tabs instead.

Video and audio work the same way: a `<video>` or `<audio>` element is given
`controls`, and a markdown link to an `.mp4`/`.mp3` wrapped around a thumbnail is
promoted to a real player with that thumbnail as its poster frame. Remote `https://`
media seeks normally; locally served media plays from the start, because the
protocol handler does not implement range requests.

---

## Reading comfortably

### The contents pill

Long documents get a floating pill at the bottom of the window showing a
**progress ring** and the section you are currently reading. It appears only when
the document actually has somewhere to be — two or more headings, and enough
content to scroll.

![The contents menu open over a document](docs/screenshots/03-contents-menu.png)

- The **ring fills** as you read, so you always know how much is left.
- The **label tracks the active heading** — the last heading above your reading line.
- Click the pill for the full outline; click an entry to jump. Smooth scrolling,
  unless the OS asks for reduced motion.
- **Dock it left or right** with the chevrons that appear on hover, so it never
  sits over the thing you are reading. The position is remembered.
- Indentation follows real heading depth, except for the common
  document-title-then-sections shape, which is flattened so the menu does not
  start two levels in.
- <kbd>Esc</kbd> closes the menu.

### Theme, width and text size

The reading area is a single warm column — light is warm paper, dark is a warm
charcoal library rather than an inverted grey. It follows your OS light/dark
setting until you say otherwise.

![Dark theme](docs/screenshots/09-reading-dark.png)

Three things are adjustable, and all of them persist:

| Control | Range | Default |
|---|---|---|
| **Reading width** | 620–1180 px | 860 px |
| **Text size** (zoom) | 0.6×–2.2× | 1.0× |
| **Theme** | light / dark | follow OS |

Narrow the column for line-by-line reading:

![Narrow reading width, pill docked left](docs/screenshots/11-narrow-width.png)

Or go wide and large for a big display:

![Wide reading width at 1.45x text](docs/screenshots/12-wide-large-text.png)

---

## Printing and PDF

<kbd>Ctrl</kbd>+<kbd>P</kbd> is deliberately **not** intercepted — you get the
real print dialog, *Print to PDF*, or *Microsoft Print to PDF*. The print
stylesheet does the work:

![The print stylesheet applied to the same document](docs/screenshots/13-print-media.png)

- All app chrome is hidden: tab bar, zoom and width controls, contents pill.
- The reading views are **independent scrollers**, so without help the printer
  only ever sees one clipped "page". Print rules unwrap them into a single flow.
- Margins come from `@page`, not the content box, so page one and page fourteen
  line up identically.
- Prose is set at 11 pt regardless of your zoom.
- **Table headers repeat on every sheet** a long table spans, and rows are kept
  whole — asking for whole tables produced sheets with a lone header and one row.
- Wide tables unroll to the full measure of the paper, because paper cannot scroll.
- Images, diagrams, code blocks, quotes and display equations are kept whole.
- Mermaid diagrams **keep their fill colours**: in a diagram the fill *is* the
  information. This is scoped to the SVG so page shading elsewhere still respects
  your "Background graphics" choice.
- Orphan and widow control keeps single lines stranded at page breaks.

A rendered sample is included: **[docs/screenshots/print-preview.pdf](docs/screenshots/print-preview.pdf)**.

---

## Keyboard shortcuts

### Global hotkey

| Keys | Action |
|---|---|
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>M</kbd> | Open mdview with a fresh tab, from anywhere |

Falls back to <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>M</kbd> then
<kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>J</kbd> if the first is already taken. The
tray tooltip always shows the combination that was actually claimed.

### In the window

| Keys | Action |
|---|---|
| <kbd>Ctrl</kbd>+<kbd>V</kbd> | Paste — text, a file, a path or a URL |
| <kbd>Ctrl</kbd>+<kbd>T</kbd> | New tab |
| <kbd>Ctrl</kbd>+<kbd>W</kbd> | Close tab |
| <kbd>Ctrl</kbd>+<kbd>=</kbd> / <kbd>Ctrl</kbd>+<kbd>-</kbd> | Zoom in / out |
| <kbd>Ctrl</kbd>+<kbd>0</kbd> | Reset zoom |
| <kbd>Ctrl</kbd>+wheel | Zoom |
| <kbd>[</kbd> / <kbd>]</kbd> | Narrower / wider reading area |
| <kbd>Alt</kbd>+<kbd>←</kbd> / <kbd>→</kbd> | Narrower / wider reading area |
| <kbd>F11</kbd> | Borderless fullscreen (native, not CSS) |
| <kbd>Esc</kbd> | Close the contents menu |
| <kbd>Ctrl</kbd>+<kbd>P</kbd> | Print / Save as PDF (native dialog) |

---

## Command line

```
mdview.exe [file.md | https://… | raw markdown text]
```

Anything that is not a flag is treated as, in order: a URL to fetch, a path to
read, or otherwise **raw markdown to render**. That last one is deliberate — you
can pass a string in from anywhere, including another program's arguments.

```powershell
mdview.exe .\notes.md
mdview.exe https://raw.githubusercontent.com/you/repo/main/README.md
mdview.exe "# Release notes`n`n- shipped the thing"
```

> mdview does not read standard input; pass the document as an argument.

| Flag | Effect |
|---|---|
| `--install-hotkey` | Install the tray helper's autostart entry, start it, and claim the global hotkey |
| `--uninstall-hotkey` | Remove the autostart entry and stop the tray helper |

Both flags work from any position and act immediately — any path alongside them is
ignored. There is no `--help` or `--version`, and unknown flags are ignored.

---

## Why it uses almost no memory

The interesting problem is not the window; it is what sits there **between**
launches. Most apps solve that with a resident process holding a webview open.
mdview does not.

- **A tiny tray helper owns the hotkey.** `mdview-tray.exe` is **0.43 MB**, has no
  webview and no async runtime, and sits in a blocking `recv` at **0% CPU**. It
  registers a real OS global hotkey and spawns the window on demand.
- **One native window, one shared WebView2.** The Edge engine is already on
  Windows, so nothing is bundled. Tabs are lightweight JS views inside it, not
  separate webviews.
- **No tokio, no async runtime.** Blocking `std` threads for fetch and read.
- **Renderer libraries are released.** When no open tab contains rendered math or
  a diagram, the inflated KaTeX/Mermaid bundle is dropped, so a long reading
  session returns to baseline instead of holding ~2.5 MB forever.
- **Hard caps** on paste (4 MB), fetch (3 MB) and served media (100 MB).
- Release profile is `opt-level="z"` with LTO, one codegen unit, and stripped
  symbols.

| Binary | Size |
|---|---|
| `mdview.exe` | 3.73 MB |
| `mdview-tray.exe` | 0.43 MB |

---

## Install

### From source

```powershell
git clone https://github.com/DiyarD/mdview.git
cd mdview
cargo build --release
.\target\release\mdview.exe --install-hotkey
```

`--install-hotkey` writes a Startup shortcut for the tray helper, launches it, and
it claims the hotkey from that moment on. Remove it with
`--uninstall-hotkey`.

> **Note:** release builds have no console attached, so `--install-hotkey` prints
> nothing and looks silent. To see its output and exit code, run it from a
> terminal explicitly:
> ```powershell
> powershell -Command "& .\target\release\mdview.exe --install-hotkey; $LASTEXITCODE"
> ```
> `.\install-hotkey.ps1` is a thin wrapper around exactly this.

You can also just run `target\release\mdview.exe` without installing anything and
pass it files, or pin it to a shortcut.

### Requirements

Windows 10/11 with WebView2 Runtime (preinstalled on current Windows). The
document renderer is plain Rust, but the window is `tao` + `wry` + WebView2, so
this is **Windows-only as written**.

---

## Build from source

```powershell
cargo run                                          # debug
cargo build --release                              # -> target\release\mdview.exe
cargo test --release                               # 20 tests + 1 doc-fixture test
```

The project has no build script and no code generation. Vendored renderer assets
are committed **gzipped**, because that is the form the binary embeds; if you ever
replace one, re-compress it in place:

```powershell
python tools/vendor-assets.py
```

---

## How it works

```
  Ctrl+Shift+M
       │
       ▼
┌──────────────┐   spawns on press, 0% CPU idle
│ mdview-tray  │──▶ mdview.exe
└──────────────┘
                    │
     ┌──────────────┴───────────────┐
     ▼                              ▼
  WebView2 IPC                 mdview:// handler
  render · read_file           KaTeX · Mermaid · hljs
  fetch_url · pick_file        local media (allowlisted)
  clipboard_files · prefs
     │
     ▼
  tab renders → enhance() → highlight → mermaid → katex → TOC
```

Markdown is parsed **in Rust**, not in the page. The page receives finished HTML
and then decorates it: highlighting, diagrams, math, link routing and the
contents pill. That split is why the parser can be trusted with edge cases and
why the renderer libraries can be swapped without touching markdown semantics.

Two implementation notes worth knowing if you read the source:

- **Everything is served over `http://mdview.localhost/…`, not `mdview://…`.**
  The page is loaded with `with_html`, so it has an opaque origin and WebView2
  refuses bare custom-scheme subresources. This is wry's documented workaround.
  The handler still accepts all three spellings.
- **Preferences live in `%APPDATA%\mdview\prefs.json`**, not in the WebView
  profile. An opaque origin cannot persist `localStorage`; the page mirrors to
  `localStorage` in a `try`/`catch` for the plain-file case, but the Rust file is
  the real store.

---

## Limits and safety

| Limit | Value |
|---|---|
| Markdown per paste / file / render | 4 MB |
| URL fetch | 3 MB |
| Media served from disk | 100 MB |
| Documents per drop / pick / paste | 8 |

On content safety: `<script>` blocks are stripped and `javascript:` URLs are
neutralised before the HTML reaches the page, the local-media protocol serves only
an allowlist of 17 media extensions, and directory traversal in a decoded path is
rejected outright. This is a viewer for documents you chose to open, not a
sandbox — treat it as such.

---

## Known rough edges

Honest list, so nobody has to rediscover these:

- **Closing the window quits mdview.** There is no minimise-to-tray for the main
  window; only the hotkey helper lives in the tray.
- **A `file:///` URL is treated as a URL, then rejected** by the HTTP(S)-only
  fetcher, so it shows an error banner and renders as text.
- **Locally served video and audio cannot seek** — the protocol handler does not
  implement range requests. Remote media seeks fine.
- **The tray helper panics** if `Ctrl+Shift+M`, `Ctrl+Alt+M` *and* `Ctrl+Shift+J`
  are all already claimed by other software.
- **No `--help` / `--version`**, and unknown flags are silently ignored.
- **No stdin support** — a document has to arrive as an argument, a paste or a drop.
- **No file-type association is registered.** mdview will happily take a `.md`
  path on the command line, but installing it does not make it the default
  handler for markdown files.
- **The loopback control ports are unauthenticated.** Any local process can make
  mdview open a tab, read a text file or issue an HTTP GET. Fine for a desktop
  viewer; do not treat it as a boundary.
- **Sanitising is deliberately narrow.** `<script>` blocks are removed and
  `javascript:` URLs are neutralised; `on*=` handlers, `<iframe>` and inline
  event attributes are not stripped. The page runs in an opaque origin, which
  limits the blast radius, but this is not a hardened HTML sanitiser.
- **Task-list checkboxes are display-only**, so pasted documents stay
  byte-identical to their source.

---

## Development

```
frontend/index.html        the entire interface: markup, CSS and JS in one file
frontend/vendor/           KaTeX, Mermaid, highlight.js — committed gzipped
src/main.rs                window, WebView2, renderer, protocol handler, CLI
src/bin/mdview-tray.rs     the tray helper that owns the global hotkey
src/appicon.rs             procedurally drawn app + tray icons (no asset files)
tools/                     asset and doc tooling
docs/demo/field-notes.md   the document every screenshot is taken from
docs/screenshots/          generated screenshots
```

There are 20 unit tests covering the renderer, table wrapping, media URL
resolution, vendored-asset inflation, preferences and the local-media gate.

### Regenerating the screenshots

The screenshots in this README are **not** mockups — they are produced from the
real renderer, the real stylesheet and the real highlight.js/KaTeX/Mermaid
bundles. The page is assembled standalone and driven in headless Chrome with a
shimmed IPC boundary, so the shipping pipeline runs unmodified.

```powershell
# 1. ask the Rust renderer for its output
$env:MDVIEW_FIXTURE_MD  = "docs\demo\field-notes.md"
$env:MDVIEW_FIXTURE_OUT = "target\fixture-body.html"
cargo test --release fixture_render -- --ignored --nocapture

# 2. assemble the page and capture every shot (plus a print PDF)
node tools/docs-screenshots.js docs\demo\field-notes.md docs\screenshots
```

`fixture_render` is `#[ignore]`d, because it is doc tooling rather than a unit
test, and it is the only thing in the repository that touches `main.rs` for
documentation purposes.

Other tools:

| Tool | Purpose |
|---|---|
| `tools/vendor-assets.py` | Re-gzip vendored renderer assets in place |
| `tools/build-preview.js` | Assemble a standalone `preview-theme.html` for CSS work |
| `tools/check-syntax.js` | Syntax-check the frontend sources |
| `tools/browse-pick.ps1` | Drive the real file-open dialog via UI Automation |
| `tools/drag-sim.ps1` | Perform a genuine OLE drag & drop with `CF_HDROP` |

---

## License

MIT — see [LICENSE](LICENSE).

Markdown parsing by [`pulldown-cmark`](https://github.com/pulldown-cmark/pulldown-cmark),
syntax highlighting by [highlight.js](https://highlightjs.org), math by
[KaTeX](https://katex.org), diagrams by [Mermaid](https://mermaid.js.org) — all
vendored under their own licences.