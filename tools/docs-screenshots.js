// Build a standalone, browser-runnable copy of the mdview interface and capture
// the screenshots used in README.md.
//
// Why this exists: the markdown is rendered by Rust (pulldown-cmark) inside the
// binary, so a plain browser cannot produce a faithful screenshot. Instead of
// reimplementing the renderer, we ask the real one for its output and shim the
// IPC boundary, so the page runs its genuine boot -> render -> enhance pipeline
// (highlight.js, KaTeX, Mermaid, contents pill) against that HTML.
//
// Step 1 (Rust, produces target/fixture-body.html):
//   cargo test --release fixture_render -- --ignored --nocapture
//     with MDVIEW_FIXTURE_MD=<doc> MDVIEW_FIXTURE_OUT=<out>
//
// Step 2 (this file): assemble the fixture, serve it over loopback HTTP and
// drive headless Chrome over CDP for full-page and per-section captures.
//
//   node tools/docs-screenshots.js <markdown-file> <output-dir>

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");
const http = require("http");
const { spawn, spawnSync } = require("child_process");

const ROOT = path.resolve(__dirname, "..");
const VENDOR = path.join(ROOT, "frontend", "vendor");
const CHROME = "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe";
const CDP_PORT = 9333;
const VIEW_W = 1400;   // CSS px of the emulated viewport
const SCALE = 2;       // device pixel ratio for crisp README images

const mdPath = path.resolve(process.argv[2] || "");
const outDir = path.resolve(process.argv[3] || path.join(ROOT, "docs", "screenshots"));
if (!mdPath || !fs.existsSync(mdPath)) {
  console.error("usage: node tools/docs-screenshots.js <markdown-file> <output-dir>");
  process.exit(1);
}
const mdDir = path.dirname(mdPath);
const mdText = fs.readFileSync(mdPath, "utf8");
// The rendered HTML comes from the Rust renderer (fixture_render test) and must
// survive the scratch-dir wipe below, so it lives beside it, not inside it.
const bodyFile = path.join(ROOT, "target", "fixture-body.html");
const work = path.join(ROOT, "target", "fixture");
if (!fs.existsSync(bodyFile)) {
  console.error("missing " + bodyFile + " - run the fixture_render test first (see header)");
  process.exit(1);
}
const bodyHtml = fs.readFileSync(bodyFile, "utf8");

fs.mkdirSync(outDir, { recursive: true });
for (const f of fs.readdirSync(outDir)) {
  if (f.endsWith(".png")) fs.rmSync(path.join(outDir, f));   // drop stale shots
}
fs.rmSync(work, { recursive: true, force: true });
fs.mkdirSync(path.join(work, "katex", "fonts"), { recursive: true });
fs.mkdirSync(path.join(work, "mermaid"), { recursive: true });

// ---- vendored renderer assets, inflated so the browser can load them --------
const gunzip = (p) => zlib.gunzipSync(fs.readFileSync(p));
fs.writeFileSync(path.join(work, "katex", "katex.min.js"), gunzip(path.join(VENDOR, "katex", "katex.min.js.gz")));
fs.writeFileSync(path.join(work, "katex", "katex.min.css"), gunzip(path.join(VENDOR, "katex", "katex.min.css.gz")));
fs.writeFileSync(path.join(work, "mermaid", "mermaid.min.js"), gunzip(path.join(VENDOR, "mermaid", "mermaid.min.js.gz")));
for (const f of fs.readdirSync(path.join(VENDOR, "katex", "fonts"))) {
  fs.copyFileSync(path.join(VENDOR, "katex", "fonts", f), path.join(work, "katex", "fonts", f));
}

// ---- page assembly ---------------------------------------------------------
function scopeCss(css, scope) {          // mirrors scope_css() in src/main.rs
  let out = "";
  for (const chunk of css.split("}")) {
    const pos = chunk.indexOf("{");
    if (pos !== -1) {
      const sel = chunk.slice(0, pos), decl = chunk.slice(pos);
      out += sel.split(",").map((s) => scope + " " + s.trim()).join(",") + decl + "}";
    } else out += chunk;
  }
  return out;
}
const hljsJs = gunzip(path.join(VENDOR, "hljs.min.js.gz")).toString("utf8").split("</script").join("<\\/script");

// Answers the two IPC calls boot() makes, so the real pipeline runs untouched.
const shim =
  "window.__MDVIEW_FIXTURE_HTML__ = " + JSON.stringify(bodyHtml) + ";\n" +
  "window.ipc = { postMessage: function (raw) {\n" +
  "  var m = JSON.parse(raw);\n" +
  "  var reply = function (ok, data) { setTimeout(function () { window.__mdview_ipc_result(m.id, ok, data); }, 0); };\n" +
  "  if (m.cmd === 'render') reply(true, window.__MDVIEW_FIXTURE_HTML__);\n" +
  "  else if (m.cmd === 'save_prefs') reply(true, true);\n" +
  "  else reply(true, true);\n" +
  "} };\n";

function buildPage(prefs) {
  let html = fs.readFileSync(path.join(ROOT, "frontend", "index.html"), "utf8");
  const init = JSON.stringify({ name: path.basename(mdPath), text: mdText, base: mdDir });
  html = html
    .split("__MDVIEW_INITIAL_JSON__").join(init)
    .split("__MDVIEW_PREFS_JSON__").join(JSON.stringify(prefs))
    .split("__MDVIEW_HLJS_JS__").join(shim + "\n" + hljsJs)
    .split("__MDVIEW_HLJS_LIGHT_CSS__").join(fs.readFileSync(path.join(VENDOR, "hljs-light.css"), "utf8"))
    .split("__MDVIEW_HLJS_DARK_CSS__").join(scopeCss(fs.readFileSync(path.join(VENDOR, "hljs-dark.css"), "utf8"), "html.dark"));
  // The real app serves these over its mdview:// handler; here they come from
  // the loopback fixture server (and /local/ mirrors mdview://local/ media).
  html = html.split("http://mdview.localhost/asset/").join("/");
  html = html.split("http://mdview.localhost/local/").join("/local/");
  return html;
}

// ---- static server ---------------------------------------------------------
const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".woff2": "font/woff2", ".png": "image/png", ".gif": "image/gif", ".jpg": "image/jpeg", ".svg": "image/svg+xml" };
const server = http.createServer((req, res) => {
  const p = decodeURIComponent(req.url.split("?")[0]);
  let file;
  if (p.startsWith("/local/")) {
    // Stand-in for mdview://local/<path>: serve the referenced media from disk.
    // Rust emits an absolute, percent-encoded Windows path here, so honour a
    // drive-letter path as-is instead of joining it onto the markdown's folder.
    const rel = p.slice("/local/".length).replace(/\//g, path.sep);
    file = /^[A-Za-z]:[\\/]/.test(rel) ? rel : path.join(mdDir, rel);
  } else {
    file = path.join(work, p === "/" ? "index.html" : p.replace(/^\//, ""));
  }
  fs.readFile(file, (err, buf) => {
    if (err) { res.writeHead(404); res.end("not found: " + file); return; }
    res.writeHead(200, { "Content-Type": MIME[path.extname(file).toLowerCase()] || "application/octet-stream" });
    res.end(buf);
  });
});

// ---- minimal CDP client ----------------------------------------------------
let msgId = 0;
function cdp(ws, method, params) {
  return new Promise((resolve, reject) => {
    const id = ++msgId;
    const onMsg = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id !== id) return;
      ws.removeEventListener("message", onMsg);
      m.error ? reject(new Error(method + ": " + JSON.stringify(m.error))) : resolve(m.result);
    };
    ws.addEventListener("message", onMsg);
    ws.send(JSON.stringify({ id, method, params: params || {} }));
  });
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function newPage(ws, url, height) {
  const { targetId } = await cdp(ws, "Target.createTarget", { url: "about:blank" });
  const { sessionId } = await cdp(ws, "Target.attachToTarget", { targetId, flatten: true });
  const send = (method, params) => new Promise((resolve, reject) => {
    const id = ++msgId;
    const onMsg = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id !== id || m.sessionId !== sessionId) return;
      ws.removeEventListener("message", onMsg);
      m.error ? reject(new Error(method + ": " + JSON.stringify(m.error))) : resolve(m.result);
    };
    ws.addEventListener("message", onMsg);
    ws.send(JSON.stringify({ id, sessionId, method, params: params || {} }));
  });
  await send("Page.enable");
  await send("Emulation.setDeviceMetricsOverride", { width: VIEW_W, height: height || 900, deviceScaleFactor: SCALE, mobile: false });
  await send("Page.navigate", { url });
  return { targetId, send };
}

async function waitForRender(send, timeoutMs) {
  // Mermaid is lazy and KaTeX is fetched, so "load" is not enough: wait for the
  // pipeline's own completion markers (or give up and use whatever rendered).
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const { result } = await send("Runtime.evaluate", {
      expression: "(!!document.querySelector('.markdown-body'))", returnByValue: true,
    });
    if (result.value) return true;
    await sleep(200);
  }
  return false;
}
async function settle(send, ms) { await sleep(ms); }

async function shoot(send, name, clip) {
  // Capture at the emulated device scale factor; clip coordinates stay in CSS
  // px. Setting clip.scale as well would multiply by SCALE twice and push the
  // region past the end of the document.
  const params = { format: "png", captureBeyondViewport: true };
  if (clip) params.clip = { ...clip, scale: 1 };
  const { data } = await send("Page.captureScreenshot", params);
  fs.writeFileSync(path.join(outDir, name), Buffer.from(data, "base64"));
  console.log("  " + name + (clip ? `  clip=${JSON.stringify(clip)}` : ""));
}

// Reading views are inner scrollers (body itself never scrolls), so a region
// past the viewport does not exist in page coordinates and would capture as
// blank. Scroll the section to the top of its view first, then crop inside the
// viewport.
async function clipSection(send, top, bottom) {
  const { result } = await send("Runtime.evaluate", {
    returnByValue: true,
    expression: `(() => {
      const pick = (s, p) => [...document.querySelectorAll(s)]
        .find(e => (e.textContent || "").trim().toLowerCase().startsWith(p));
      const t = pick(${JSON.stringify(top.selector)}, ${JSON.stringify(top.prefix.toLowerCase())});
      const b = pick(${JSON.stringify(bottom.selector)}, ${JSON.stringify((bottom.prefix || "").toLowerCase())});
      if (!t || !b) return { error: 'no match for ' + (!t ? top.selector : bottom.selector) };
      t.scrollIntoView({ block: 'start' });
      const tr = t.getBoundingClientRect(), br = b.getBoundingClientRect();
      const vh = window.innerHeight;
      const padTop = 16, padBottom = 24, padX = 26;
      const y = Math.max(0, tr.top - padTop);
      const bottomEdge = Math.min(vh, Math.max(br.bottom, tr.bottom) + padBottom);
      return {
        x: Math.max(0, Math.min(tr.left, br.left) - padX),
        y,
        width: Math.min(window.innerWidth, Math.max(tr.width, br.width) + padX * 2),
        height: Math.max(40, bottomEdge - y),
      };
    })()`,
  });
  if (result.value && result.value.error) return { error: result.value.error };
  return result.value;
}
async function docHeight(send) {
  const { result } = await send("Runtime.evaluate", {
    returnByValue: true,
    expression: `Math.max(
      document.body.scrollHeight,
      document.documentElement.scrollHeight,
      document.querySelector('.view.active').scrollHeight
    )`,
  });
  return result.value;
}

// ---- print / PDF -----------------------------------------------------------
// Real @media print output: Chrome's printToPDF runs the same stylesheet the
// app's Ctrl+P path would, so this doubles as proof the print rules work.
async function printPdf(ws, port, outFile) {
  fs.writeFileSync(path.join(work, "index.html"), buildPage(LIGHT));
  const page = await newPage(ws, "http://127.0.0.1:" + port + "/index.html");
  await waitForRender(page.send, 25000);
  await settle(page.send, 1800);
  const { data } = await page.send("Page.printToPDF", {
    printBackground: true,
    preferCSSPageSize: true,
    marginTop: 0, marginBottom: 0, marginLeft: 0, marginRight: 0,
  });
  fs.writeFileSync(outFile, Buffer.from(data, "base64"));
  await cdp(ws, "Target.closeTarget", { targetId: page.targetId });
  console.log("  " + path.basename(outFile));
}

// ---- shots -----------------------------------------------------------------
// `top`/`bottom` bound the clip: each is {selector, prefix} matched against the
// rendered page. Omitting `top` starts at `bottom` itself.
const LIGHT = { theme: "light", fontScale: 1.0, readWidth: 860, tocPos: "center" };
const DARK = { theme: "dark", fontScale: 1.0, readWidth: 860, tocPos: "center" };
const SHOTS = [
  { name: "01-reading-light.png", prefs: LIGHT, mode: "top" },
  { name: "02-full-document.png", prefs: { theme: "light", fontScale: 0.62, readWidth: 1000, tocPos: "center" }, mode: "full" },
  { name: "03-contents-menu.png", prefs: LIGHT, mode: "toc", height: 1240 },
  { name: "04-code.png", prefs: LIGHT, mode: "clip",
    top: { selector: "h2", prefix: "Code" }, bottom: { selector: "pre", prefix: "fn main()" } },
  { name: "05-math.png", prefs: LIGHT, mode: "clip",
    top: { selector: "h2", prefix: "Math" }, bottom: { selector: "p", prefix: "Long display equations" } },
  { name: "06-diagram.png", prefs: LIGHT, mode: "clip",
    top: { selector: "h2", prefix: "Diagrams" }, bottom: { selector: ".mermaid svg", prefix: "" } },
  { name: "07-tables.png", prefs: LIGHT, mode: "clip",
    top: { selector: "h2", prefix: "Tables that fit" }, bottom: { selector: "table", prefix: "shortcut" } },
  { name: "08-media.png", prefs: LIGHT, mode: "clip",
    top: { selector: "h2", prefix: "Images and video" }, bottom: { selector: "img", prefix: "" } },
  { name: "09-reading-dark.png", prefs: DARK, mode: "top" },
  { name: "10-dark-diagram.png", prefs: DARK, mode: "clip",
    top: { selector: "h2", prefix: "Diagrams" }, bottom: { selector: ".mermaid svg", prefix: "" } },
  { name: "11-narrow-width.png", prefs: { theme: "light", fontScale: 1.0, readWidth: 620, tocPos: "left" }, mode: "top" },
  { name: "12-wide-large-text.png", prefs: { theme: "light", fontScale: 1.45, readWidth: 1180, tocPos: "center" }, mode: "top" },
  { name: "13-print-media.png", prefs: LIGHT, mode: "print" },
];

(async () => {
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;

  const userDir = path.join(ROOT, "target", "chrome-fixture-profile");
  fs.rmSync(userDir, { recursive: true, force: true });
  const chrome = spawn(CHROME, [
    "--headless=new", "--disable-gpu", "--hide-scrollbars", "--no-first-run",
    "--no-default-browser-check", "--disable-extensions", "--force-device-scale-factor=1",
    "--remote-debugging-port=" + CDP_PORT, "--user-data-dir=" + userDir, "about:blank",
  ], { stdio: "ignore" });

  let wsUrl = null;
  for (let i = 0; i < 60 && !wsUrl; i++) {
    await sleep(300);
    try {
      const r = spawnSync("powershell", ["-NoProfile", "-Command",
        "(Invoke-RestMethod http://127.0.0.1:" + CDP_PORT + "/json/version).webSocketDebuggerUrl"],
        { encoding: "utf8" });
      const line = (r.stdout || "").trim().split(/\r?\n/).filter(Boolean).pop();
      if (line && line.startsWith("ws://")) wsUrl = line;
    } catch (_) {}
  }
  if (!wsUrl) { console.error("could not reach Chrome DevTools"); chrome.kill(); server.close(); process.exit(1); }

  const ws = new WebSocket(wsUrl);
  await new Promise((r, j) => { ws.addEventListener("open", r); ws.addEventListener("error", j); });

  for (const shot of SHOTS) {
    fs.writeFileSync(path.join(work, "index.html"), buildPage(shot.prefs));
    // A taller viewport lets the contents menu and clipped sections fit whole.
    const page = await newPage(ws, "http://127.0.0.1:" + port + "/index.html", shot.height);
    const ok = await waitForRender(page.send, 25000);
    if (!ok) console.warn("  ! render marker missing for " + shot.name);
    await settle(page.send, 1800);           // mermaid + katex settle

    if (shot.mode === "top") {
      await shoot(page.send, shot.name);
    } else if (shot.mode === "full") {
      const h = await docHeight(page.send);
      await page.send("Emulation.setDeviceMetricsOverride", { width: VIEW_W, height: Math.min(h, 30000), deviceScaleFactor: SCALE, mobile: false });
      await settle(page.send, 700);
      await shoot(page.send, shot.name);
    } else if (shot.mode === "toc") {
      await page.send("Runtime.evaluate", { expression: "setTocOpen(true)" });
      await settle(page.send, 600);
      await shoot(page.send, shot.name);
    } else if (shot.mode === "clip") {
      const clip = await clipSection(page.send, shot.top, shot.bottom);
      if (clip.error) console.warn("  ! " + shot.name + ": " + clip.error);
      else await shoot(page.send, shot.name, clip);
    } else if (shot.mode === "print") {
      // Emulate print media so the @media print rules apply, then capture the
      // whole unwrapped sheet (chrome hidden, scroller un-clipped).
      await page.send("Emulation.setEmulatedMedia", { media: "print" });
      await settle(page.send, 900);
      const h = await docHeight(page.send);
      await page.send("Emulation.setDeviceMetricsOverride", { width: 1100, height: Math.min(h, 30000), deviceScaleFactor: 1, mobile: false });
      await settle(page.send, 900);
      const clip = { x: 0, y: 0, width: 1100, height: Math.min(h, 2100), scale: 1 };
      await shoot(page.send, shot.name, clip);
    }
    await cdp(ws, "Target.closeTarget", { targetId: page.targetId });
  }

  await printPdf(ws, port, path.join(outDir, "print-preview.pdf"));

  ws.close();
  chrome.kill();
  server.close();
  console.log("wrote " + SHOTS.length + " screenshots + print-preview.pdf to " + outDir);
})();