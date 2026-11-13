// Behavioral test of the real mdview page: the same browser-runnable assembly
// the README screenshots use (page-harness.js), with the IPC boundary shimmed
// stateful — a fake "Rust side" document store — so the genuine
// boot -> paste -> render -> save -> rename -> search -> open pipeline runs end
// to end. No reimplemented logic: assertions are made against the DOM and the
// IPC messages the page actually sends.
//
//   node tools/test-ui.js
//
// Exits non-zero if any behavior the user depends on is broken.

const fs = require("fs");
const path = require("path");
const {
  ROOT, hljsBundle, buildPage, startServer, waitFor, sleep, spawnChrome, newPage,
} = require("./page-harness");

const CDP_PORT = 9334;
const FIXTURE_HTML = "<h1>UI test doc</h1><p>alpha bravo charlie</p>";

// A faithful stand-in for the Rust document store: in-memory, byte-exact
// round-trip, same commands and payloads the real IPC layer answers.
const shim =
  "var __store = [];\n" +
  "var __nextId = 1;\n" +
  "function __now() { return Math.floor(Date.now() / 1000); }\n" +
  "window.__UI_TEST_SENT__ = [];\n" +
  "window.ipc = { postMessage: function (raw) {\n" +
  "  var m = JSON.parse(raw);\n" +
  "  window.__UI_TEST_SENT__.push(m);\n" +
  "  var reply = function (ok, data) { setTimeout(function () { window.__mdview_ipc_result(m.id, ok, data); }, 0); };\n" +
  "  if (m.cmd === 'render') reply(true, " + JSON.stringify(FIXTURE_HTML) + ");\n" +
  "  else if (m.cmd === 'save_prefs') reply(true, true);\n" +
  "  else if (m.cmd === 'set_title') reply(true, 'true');\n" +
  "  else if (m.cmd === 'clipboard_files') reply(true, '[]');\n" +
  "  else if (m.cmd === 'release_asset') reply(true, true);\n" +
  "  else if (m.cmd === 'recent_documents') {\n" +
  "    reply(true, JSON.stringify(__store.slice().sort(function (a, b) { return b.updated_at - a.updated_at; }).map(function (d) {\n" +
  "      return { id: d.id, title: d.title, preview: d.preview, updated_at: d.updated_at };\n" +
  "    })));\n" +
  "  }\n" +
  "  else if (m.cmd === 'search_documents') {\n" +
  "    var q = String(m.query || '').toLowerCase();\n" +
  "    reply(true, JSON.stringify(__store.filter(function (d) {\n" +
  "      return d.title.toLowerCase().indexOf(q) >= 0 || d.text.toLowerCase().indexOf(q) >= 0;\n" +
  "    }).map(function (d) { return { id: d.id, title: d.title, snippet: d.text.slice(0, 120), updated_at: d.updated_at, score: 0 }; })));\n" +
  "  }\n" +
  "  else if (m.cmd === 'save_document' || m.cmd === 'auto_save_document') {\n" +
  "    var existing = null;\n" +
  "    if (m.doc_id != null) existing = __store.find(function (d) { return d.id === m.doc_id; }) || null;\n" +
  "    else if (m.source_ref) existing = __store.find(function (d) { return d.source_ref === m.source_ref && d.source_kind === m.source_kind; }) || null;\n" +
  "    if (existing) {\n" +
  "      existing.title = m.title; existing.text = m.text; existing.updated_at = __now();\n" +
  "      reply(true, JSON.stringify({ id: existing.id, title: existing.title, updated_at: existing.updated_at }));\n" +
  "    } else {\n" +
  "      var d = { id: __nextId++, title: m.title, text: m.text, base: m.base || '', source_kind: m.source_kind || 'paste',\n" +
  "                 source_ref: m.source_ref || '', preview: m.text.slice(0, 200), updated_at: __now() };\n" +
  "      __store.push(d);\n" +
  "      reply(true, JSON.stringify({ id: d.id, title: d.title, updated_at: d.updated_at }));\n" +
  "    }\n" +
  "  }\n" +
  "  else if (m.cmd === 'open_document') {\n" +
  "    var d = __store.find(function (x) { return x.id === m.doc_id; });\n" +
  "    if (!d) { reply(false, 'no such document'); return; }\n" +
  "    reply(true, JSON.stringify({ id: d.id, title: d.title, text: d.text, base: d.base,\n" +
  "      source_kind: d.source_kind, source_ref: d.source_ref, created_at: 0, updated_at: d.updated_at, open_count: 1 }));\n" +
  "  }\n" +
  "  else if (m.cmd === 'rename_document') {\n" +
  "    var d = __store.find(function (x) { return x.id === m.doc_id; });\n" +
  "    if (d) d.title = m.title;\n" +
  "    reply(true, 'true');\n" +
  "  }\n" +
  "  else if (m.cmd === 'delete_document') {\n" +
  "    var i = __store.findIndex(function (x) { return x.id === m.doc_id; });\n" +
  "    if (i >= 0) { __store.splice(i, 1); reply(true, 'true'); } else reply(true, 'false');\n" +
  "  }\n" +
  "  else if (m.cmd === 'find_similar') {\n" +
  "    if (String(m.text || '').indexOf('sim-target-marker') >= 0 && __store.length) {\n" +
  "      var t = __store[__store.length - 1];\n" +
  "      reply(true, JSON.stringify({ id: t.id, title: t.title, updated_at: t.updated_at, source_kind: t.source_kind,\n" +
  "        source_ref: t.source_ref, score: 0.9, exact: false }));\n" +
  "    } else reply(true, 'null');\n" +
  "  }\n" +
  "  else reply(true, true);\n" +
  "} };\n";

const work = path.join(ROOT, "target", "ui-test");
const userDir = path.join(ROOT, "target", "chrome-ui-test-profile");
const results = [];
function report(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log((ok ? "  PASS" : "  FAIL") + "  " + name + (ok || !detail ? "" : " — " + detail));
}

async function evalValue(send, expression) {
  const { result } = await send("Runtime.evaluate", {
    expression: `(() => { try { return (${expression}); } catch (e) { return { __error: String(e && e.message || e) }; } })()`,
    returnByValue: true,
  });
  return result.value;
}

async function main() {
  fs.rmSync(work, { recursive: true, force: true });
  fs.mkdirSync(work, { recursive: true });
  const html = fs.readFileSync(path.join(ROOT, "frontend", "index.html"), "utf8");
  const hljsJs = hljsBundle();
  fs.writeFileSync(path.join(work, "index.html"), buildPage(html, {
    initial: { name: "", text: "", base: "", source: "" },
    prefs: { theme: "light", fontScale: 1.0, readWidth: 860, tocPos: "center" },
    hljsJs: shim + "\n" + hljsJs,
  }));

  const server = startServer({ work, rootForLocal: work });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;

  const { chrome, wsUrl } = await spawnChrome({ userDir, cdpPort: CDP_PORT });
  if (!wsUrl) { console.error("could not reach Chrome DevTools"); chrome.kill(); server.close(); process.exit(1); }
  const ws = new WebSocket(wsUrl);
  await new Promise((r, j) => { ws.addEventListener("open", r); ws.addEventListener("error", j); });

  const page = await newPage(ws, "http://127.0.0.1:" + port + "/index.html");
  const { send } = page;

  try {
    // 1. Boot: one empty tab, saved-documents section shows the empty state.
    report("boot creates a tab", await waitFor(send, "document.querySelectorAll('.tab').length === 1", 10000));
    report("welcome shows empty saved state", await waitFor(send,
      "!!document.querySelector('.saved-empty') && document.querySelector('.saved-empty').textContent.indexOf('No saved documents') >= 0", 5000));

    // 2. Paste a document — the real paste path.
    report("paste renders a tab", await (async () => {
      await evalValue(send, `(() => {
        const dt = new DataTransfer();
        dt.setData("text/plain", "# UI test doc\\n\\nalpha bravo charlie");
        document.dispatchEvent(new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true }));
        return true;
      })()`);
      return waitFor(send, "!!document.querySelector('.markdown-body')", 10000);
    })());
    const tabTitle = await evalValue(send, "(document.querySelector('.tab .t') || {}).textContent || ''");
    report("tab is titled from the markdown heading", tabTitle === "UI test doc", "got: " + tabTitle);

    // 3. Save (Ctrl+S button). The exact regression that made this dead: the
    //    document id must not clobber the bridge's request id.
    await evalValue(send, "document.querySelector('#save').click()");
    report("save marks the button saved", await waitFor(send, "document.querySelector('#save').classList.contains('saved')", 5000));
    const sentSave = await evalValue(send, "window.__UI_TEST_SENT__.filter(function (m) { return m.cmd === 'save_document'; })");
    report("save_document carries doc_id (never the bridge id)",
      Array.isArray(sentSave) && sentSave.length === 1 && sentSave[0].doc_id === null,
      JSON.stringify(sentSave));

    // 4. Double-click the tab to rename → Enter commits.
    await evalValue(send, "(() => { const t = document.querySelector('.tab'); t.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()");
    report("dblclick opens the inline rename input", await waitFor(send, "!!document.querySelector('input.tab-input')", 5000));
    await evalValue(send, "(() => { const i = document.querySelector('input.tab-input'); i.value = 'Renamed doc'; i.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true })); return true; })()");
    report("Enter commits the rename", await waitFor(send, "document.querySelector('.tab .t') && document.querySelector('.tab .t').textContent === 'Renamed doc'", 5000));
    const sentRename = await evalValue(send, "window.__UI_TEST_SENT__.filter(function (m) { return m.cmd === 'rename_document'; })");
    report("rename_document goes to the saved document",
      Array.isArray(sentRename) && sentRename.length === 1 && sentRename[0].doc_id === 1 && sentRename[0].title === "Renamed doc",
      JSON.stringify(sentRename));
    const sentTitle = await evalValue(send, "window.__UI_TEST_SENT__.filter(function (m) { return m.cmd === 'set_title'; })");
    report("window title follows the rename", Array.isArray(sentTitle) && sentTitle.some((m) => m.title === "Renamed doc"));

    // 5. New tab: the welcome screen shows the saved document.
    await evalValue(send, "document.querySelector('#newtab').click()");
    report("welcome lists the saved document", await waitFor(send,
      "document.querySelector('.saved-item .saved-title') && document.querySelector('.saved-item .saved-title').textContent === 'Renamed doc'", 5000));

    // 6. Search it — one query box for title and content, with debounce.
    await evalValue(send, "(() => { const s = document.querySelector('.saved-search'); s.value = 'renamed'; s.dispatchEvent(new Event('input', { bubbles: true })); return true; })()");
    report("search fires after the debounce", await waitFor(send,
      "window.__UI_TEST_SENT__.some(function (m) { return m.cmd === 'search_documents' && m.query === 'renamed'; })", 5000));
    const itemAfterSearch = await evalValue(send, "!!document.querySelector('.saved-item')");
    report("matching result stays visible", itemAfterSearch);

    // 7. Click it — it must open.
    const beforeTabs = await evalValue(send, "document.querySelectorAll('.tab').length");
    await evalValue(send, "document.querySelector('.saved-item .saved-main').click()");
    report("open_document fires for the clicked result", await waitFor(send,
      "window.__UI_TEST_SENT__.some(function (m) { return m.cmd === 'open_document' && m.doc_id === 1; })", 5000));
    report("the saved document opens in a new tab", await waitFor(send, "document.querySelectorAll('.tab').length > " + beforeTabs, 5000));
    report("the opened tab renders the saved content", await waitFor(send,
      "document.querySelector('.view.active .markdown-body') && document.querySelector('.view.active .markdown-body').textContent.indexOf('alpha bravo') >= 0", 5000));

    // 8. Similarity offer: a doc that "matches" the saved one must offer it, not swap silently.
    await evalValue(send, `(() => {
      const dt = new DataTransfer();
      dt.setData("text/plain", "# Sim doc\\n\\nsim-target-marker alpha bravo charlie");
      document.dispatchEvent(new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true }));
      return true;
    })()`);
    report("similarity banner appears", await waitFor(send, "!!document.querySelector('.similar')", 8000));
    const banner = await evalValue(send, "document.querySelector('.similar') ? document.querySelector('.similar').textContent : ''");
    report("banner names the saved document", banner.indexOf("Renamed doc") >= 0, "banner: " + banner);
    await evalValue(send, "(() => { const b = Array.from(document.querySelectorAll('.similar .btn')).find(function (x) { return x.textContent === 'Open saved'; }); if (b) b.click(); return true; })()");
    report("clicking Open saved swaps in the saved document", await waitFor(send,
      "document.querySelector('.view.active .markdown-body') && document.querySelector('.view.active .markdown-body').textContent.indexOf('alpha bravo') >= 0", 8000));
    report("banner is dismissed after opening", (await evalValue(send, "!document.querySelector('.similar')")) === true);

    // 9. Ctrl+W: closing a tab must remove its button from the strip.
    const pressW = async () => {
      await evalValue(send, "document.dispatchEvent(new KeyboardEvent('keydown', { key: 'w', ctrlKey: true, bubbles: true }))");
      await sleep(80);
    };
    await evalValue(send, "document.querySelector('#newtab').click()");
    await evalValue(send, "document.querySelector('#newtab').click()");
    const beforeClose = await evalValue(send, "document.querySelectorAll('.tab').length");
    await pressW();
    report("Ctrl+W removes the closed tab from the strip",
      (await evalValue(send, "document.querySelectorAll('.tab').length")) === beforeClose - 1 &&
      (await evalValue(send, "document.querySelectorAll('.view').length")) === beforeClose - 1);
    await pressW();
    report("Ctrl+W again: strip still matches the tab array",
      (await evalValue(send, "document.querySelectorAll('.tab').length")) === beforeClose - 2);
    // Close everything down; the last close must spawn one fresh empty tab.
    await pressW(); await pressW(); await pressW(); await pressW();
    report("closing the last tab creates a fresh empty tab",
      (await evalValue(send, "document.querySelectorAll('.tab').length")) === 1 &&
      (await evalValue(send, "!!document.querySelector('.drop')")) === true);

    // 9. Bridge contract: every request id is the bridge's own string — the
    //    exact regression that hung every DB call.
    const ids = await evalValue(send, "window.__UI_TEST_SENT__.map(function (m) { return m.id; })");
    report("every request id is the bridge's own (rN)",
      Array.isArray(ids) && ids.length > 0 && ids.every((id) => typeof id === "string" && /^r\d+$/.test(id)),
      JSON.stringify(ids));

    // 10. Delete a saved document from the home screen, from the row itself.
    report("home lists the saved document with a delete button", await waitFor(send,
      "!!document.querySelector('.saved-item .saved-x')", 5000));
    await evalValue(send, "(() => { document.querySelector('.saved-item .saved-x').click(); return true; })()");
    report("first click arms the confirm", (await evalValue(send, "(document.querySelector('.saved-item .saved-x') || {}).textContent")) === "Delete?");
    await evalValue(send, "(() => { document.querySelector('.saved-item .saved-x').click(); return true; })()");
    report("second click deletes the row", await waitFor(send,
      "!document.querySelector('.saved-item') && !!document.querySelector('.saved-empty')", 5000));
    const sentDelete = await evalValue(send, "window.__UI_TEST_SENT__.filter(function (m) { return m.cmd === 'delete_document'; })");
    report("delete_document targets the saved row",
      Array.isArray(sentDelete) && sentDelete.length === 1 && sentDelete[0].doc_id === 1,
      JSON.stringify(sentDelete));

    const failed = results.filter((r) => !r.ok).length;
    console.log("");
    console.log(failed === 0 ? "ALL " + results.length + " UI BEHAVIOR CHECKS PASSED" : failed + "/" + results.length + " CHECKS FAILED");
    process.exitCode = failed === 0 ? 0 : 1;
  } catch (e) {
    console.error("test-ui failed with:", e);
    process.exitCode = 1;
  } finally {
    try { await send("Browser.close"); } catch (_) {}
    ws.close();
    chrome.kill();
    server.close();
  }
}

main().catch((e) => { console.error(e); process.exit(1); });
