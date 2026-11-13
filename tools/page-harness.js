// Shared harness for tools that drive the real mdview page in headless Chrome.
//
// Extracted from docs-screenshots.js so there is exactly one implementation of:
//   - assembling a browser-runnable copy of frontend/index.html (whose
//     __MDVIEW_*__ placeholders Rust fills in the real binary),
//   - serving it over loopback HTTP with an mdview://local/ stand-in,
//   - spawning headless Chrome and talking to it over CDP.
//
// Used by docs-screenshots.js (README screenshots) and test-ui.js (behavior).
const fs = require("fs");
const path = require("path");
const zlib = require("zlib");
const http = require("http");
const { spawn, spawnSync } = require("child_process");

const ROOT = path.resolve(__dirname, "..");
const VENDOR = path.join(ROOT, "frontend", "vendor");
const CHROME = "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe";
const VIEW_W = 1400; // CSS px of the emulated viewport
const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".woff2": "font/woff2", ".png": "image/png", ".gif": "image/gif", ".jpg": "image/jpeg", ".svg": "image/svg+xml" };

const gunzip = (p) => zlib.gunzipSync(fs.readFileSync(p));

// ---- shared page assembly ---------------------------------------------------
// Replaces every placeholder Rust fills in build_html(), then re-points the
// mdview.localhost asset/local prefixes at the loopback fixture server.
function scopeCss(css, scope) { // mirrors scope_css() in src/main.rs
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
// highlight.js, inflated and escaped exactly as the binary inlines it
function hljsBundle() {
  return gunzip(path.join(VENDOR, "hljs.min.js.gz")).toString("utf8").split("</script").join("<\\/script");
}
function hljsLightCss() { return fs.readFileSync(path.join(VENDOR, "hljs-light.css"), "utf8"); }
function hljsDarkScopedCss() { return scopeCss(fs.readFileSync(path.join(VENDOR, "hljs-dark.css"), "utf8"), "html.dark"); }

function buildPage(html, { initial, prefs, hljsJs }) {
  html = html
    .split("__MDVIEW_INITIAL_JSON__").join(JSON.stringify(initial))
    .split("__MDVIEW_PREFS_JSON__").join(JSON.stringify(prefs))
    .split("__MDVIEW_HLJS_JS__").join(hljsJs)
    .split("__MDVIEW_HLJS_LIGHT_CSS__").join(hljsLightCss())
    .split("__MDVIEW_HLJS_DARK_CSS__").join(hljsDarkScopedCss());
  return html
    .split("http://mdview.localhost/asset/").join("/")
    .split("http://mdview.localhost/local/").join("/local/");
}

// ---- loopback static server -------------------------------------------------
function startServer({ work, rootForLocal }) {
  return http.createServer((req, res) => {
    const p = decodeURIComponent(req.url.split("?")[0]);
    let file;
    if (p.startsWith("/local/")) {
      // Stand-in for mdview://local/<path>: serve the referenced media from disk.
      // Rust emits an absolute, percent-encoded Windows path here, so honour a
      // drive-letter path as-is instead of joining it onto the markdown's folder.
      const rel = p.slice("/local/".length).replace(/\//g, path.sep);
      file = /^[A-Za-z]:[\\/]/.test(rel) ? rel : path.join(rootForLocal, rel);
    } else {
      file = path.join(work, p === "/" ? "index.html" : p.replace(/^\//, ""));
    }
    fs.readFile(file, (err, buf) => {
      if (err) { res.writeHead(404); res.end("not found: " + file); return; }
      res.writeHead(200, { "Content-Type": MIME[path.extname(file).toLowerCase()] || "application/octet-stream" });
      res.end(buf);
    });
  });
}

// ---- minimal CDP client -----------------------------------------------------
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
  await send("Emulation.setDeviceMetricsOverride", { width: VIEW_W, height: height || 900, deviceScaleFactor: 1, mobile: false });
  await send("Page.navigate", { url });
  return { targetId, send };
}

// Generic poll: resolves when the expression evaluates truthy in the page.
async function waitFor(send, expression, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const { result } = await send("Runtime.evaluate", { expression: `(!!(${expression}))`, returnByValue: true });
    if (result.value) return true;
    await sleep(100);
  }
  return false;
}
async function waitForRender(send, timeoutMs) {
  return waitFor(send, "document.querySelector('.markdown-body')", timeoutMs);
}
async function settle(send, ms) { await sleep(ms); }

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

// Spawn headless Chrome and wait for its DevTools endpoint.
// Returns { chrome, wsUrl } — wsUrl may be null if Chrome never came up.
async function spawnChrome({ userDir, cdpPort }) {
  fs.rmSync(userDir, { recursive: true, force: true });
  const chrome = spawn(CHROME, [
    "--headless=new", "--disable-gpu", "--hide-scrollbars", "--no-first-run",
    "--no-default-browser-check", "--disable-extensions", "--force-device-scale-factor=1",
    "--remote-debugging-port=" + cdpPort, "--user-data-dir=" + userDir, "about:blank",
  ], { stdio: "ignore" });
  let wsUrl = null;
  for (let i = 0; i < 60 && !wsUrl; i++) {
    await sleep(300);
    try {
      const r = spawnSync("powershell", ["-NoProfile", "-Command",
        "(Invoke-RestMethod http://127.0.0.1:" + cdpPort + "/json/version).webSocketDebuggerUrl"],
        { encoding: "utf8" });
      const line = (r.stdout || "").trim().split(/\r?\n/).filter(Boolean).pop();
      if (line && line.startsWith("ws://")) wsUrl = line;
    } catch (_) {}
  }
  return { chrome, wsUrl };
}

module.exports = {
  ROOT, VENDOR, CHROME, VIEW_W, MIME, sleep,
  gunzip, scopeCss, hljsBundle, hljsLightCss, hljsDarkScopedCss,
  buildPage, startServer,
  cdp, newPage, waitFor, waitForRender, settle, docHeight, spawnChrome,
};
