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
  "var __annotations = [];\n" +
  "var __annNextId = 1;\n" +
  "function __now() { return Math.floor(Date.now() / 1000); }\n" +
  "window.__UI_TEST_SENT__ = [];\n" +
  "window.ipc = { postMessage: function (raw) {\n" +
  "  var m = JSON.parse(raw);\n" +
  "  window.__UI_TEST_SENT__.push(m);\n" +
  "  var reply = function (ok, data) {\n" +
  "    if (typeof window.__UI_TEST_SLOW_ADD__ !== 'boolean') window.__UI_TEST_SLOW_ADD__ = false;\n" +
  "    setTimeout(function () { window.__mdview_ipc_result(m.id, ok, data); }, window.__UI_TEST_SLOW_ADD__ ? 350 : 0);\n" +
  "  };\n" +
  "  if (m.cmd === 'render') reply(true, " + JSON.stringify(FIXTURE_HTML) + ");\n" +
  "  else if (m.cmd === 'save_prefs') reply(true, true);\n" +
  "  else if (m.cmd === 'set_title') reply(true, 'true');\n" +
  "  else if (m.cmd === 'clipboard_files') reply(true, '[]');\n" +
  "  else if (m.cmd === 'clipboard_text') reply(true, '# Button paste doc\\n\\nbutton paste body');\n" +
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
  "    if (i >= 0) {\n" +
  "      __store.splice(i, 1);\n" +
  "      for (var j = __annotations.length - 1; j >= 0; j--) { if (__annotations[j].document_id === m.doc_id) __annotations.splice(j, 1); }\n" +
  "      reply(true, 'true');\n" +
  "    } else reply(true, 'false');\n" +
  "  }\n" +
  "  else if (m.cmd === 'annotation_add') {\n" +
  "    var doc = __store.find(function (d) { return d.id === m.doc_id; });\n" +
  "    if (!doc) { reply(false, 'document not found'); return; }\n" +
  "    if (!String(m.exact || '').trim()) { reply(false, 'annotation has no selected text'); return; }\n" +
  "    if (!(m.start >= 0) || !(m.end > m.start)) { reply(false, 'annotation offsets are out of range'); return; }\n" +
  "    if (['yellow', 'green', 'blue', 'pink', 'purple'].indexOf(String(m.color || '')) < 0) { reply(false, 'unsupported annotation color'); return; }\n" +
  "    var a = { id: __annNextId++, document_id: m.doc_id, exact: m.exact, prefix: m.prefix || '', suffix: m.suffix || '',\n" +
  "              start: m.start, end: m.end, color: m.color, note: m.note || '', state: 'active', created_at: __now(), updated_at: __now() };\n" +
  "    __annotations.push(a);\n" +
  "    reply(true, JSON.stringify(a));\n" +
  "  }\n" +
  "  else if (m.cmd === 'annotation_list') {\n" +
  "    reply(true, JSON.stringify(__annotations.filter(function (a) { return a.document_id === m.doc_id; })\n" +
  "      .sort(function (a, b) { return a.start - b.start || a.id - b.id; })));\n" +
  "  }\n" +
  "  else if (m.cmd === 'annotation_update') {\n" +
  "    var u1 = __annotations.find(function (a) { return a.id === m.annotation_id; });\n" +
  "    if (!u1) { reply(true, 'false'); return; }\n" +
  "    u1.color = m.color; u1.note = m.note || ''; u1.updated_at = __now();\n" +
  "    reply(true, 'true');\n" +
  "  }\n" +
  "  else if (m.cmd === 'annotation_refresh_anchor') {\n" +
  "    var u2 = __annotations.find(function (a) { return a.id === m.annotation_id; });\n" +
  "    if (!u2) { reply(true, 'false'); return; }\n" +
  "    var changed = u2.prefix !== m.prefix || u2.suffix !== m.suffix || u2.start !== m.start || u2.end !== m.end || u2.state !== m.state;\n" +
  "    if (changed) { u2.prefix = m.prefix; u2.suffix = m.suffix; u2.start = m.start; u2.end = m.end; u2.state = m.state; u2.updated_at = __now(); }\n" +
  "    reply(true, changed ? 'true' : 'false');\n" +
  "  }\n" +
  "  else if (m.cmd === 'annotation_delete') {\n" +
  "    var k = __annotations.findIndex(function (a) { return a.id === m.annotation_id; });\n" +
  "    if (k >= 0) { __annotations.splice(k, 1); reply(true, 'true'); } else reply(true, 'false');\n" +
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

    // 1b. Paste now reads the clipboard natively (Win32 via Rust), not the
    //     permission-blocked browser API.
    report("Paste now renders the clipboard document", await (async () => {
      await evalValue(send, "document.querySelector('[data-act=\"paste\"]').click()");
      return waitFor(send, "!!document.querySelector('.markdown-body')", 10000);
    })());
    const buttonPasteTitle = await evalValue(send, "(document.querySelector('.tab .t') || {}).textContent || ''");
    report("Paste now titles the tab from the document", buttonPasteTitle === "Button paste doc", "got: " + buttonPasteTitle);

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
    //    document id must not clobber the bridge's request id. The saved class
    //    must not resize/shift the button (class-collision regression).
    const size0 = await evalValue(send, "(() => { const r = document.querySelector('#save').getBoundingClientRect(); return [Math.round(r.height), Math.round(r.top)]; })()");
    await evalValue(send, "document.querySelector('#save').click()");
    report("save marks the button saved", await waitFor(send, "document.querySelector('#save').classList.contains('saved')", 5000));
    const size1 = await evalValue(send, "(() => { const r = document.querySelector('#save').getBoundingClientRect(); return [Math.round(r.height), Math.round(r.top)]; })()");
    report("saved state keeps the button size and position",
      Array.isArray(size0) && Array.isArray(size1) && size0[0] === size1[0] && size0[1] === size1[1],
      "before: " + JSON.stringify(size0) + " after: " + JSON.stringify(size1));
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

    // 7b. Selecting text reveals a proposal button just below the caret.
    //     One click stores a default-colored highlight — no dialog. Clicking
    //     the highlight reopens the collapsed palette. Auto-save comes first.
    const selectText = `(() => {
      const p = document.querySelector('.view.active .markdown-body p');
      const tn = p && p.firstChild;
      if (!tn) return false;
      const r = document.createRange();
      r.selectNodeContents(tn);
      const s = window.getSelection();
      s.removeAllRanges();
      s.addRange(r);
      document.dispatchEvent(new PointerEvent('pointerup', { button: 0, bubbles: true, clientX: 40, clientY: 40 }));
      return true;
    })()`;
    const clickOnHighlight = `(() => {
      const p = document.querySelector('.view.active .markdown-body p');
      const r = p.getBoundingClientRect();
      document.dispatchEvent(new MouseEvent('click', { clientX: r.left + 30, clientY: r.top + r.height / 2, bubbles: true }));
      return true;
    })()`;
    await evalValue(send, selectText);
    report("selection reveals the propose button (not the editor)", await waitFor(send,
      "!document.querySelector('#annPropose').hidden && !!document.querySelector('#annEditor') && document.querySelector('#annEditor').hidden", 5000));
    await evalValue(send, "(() => { document.dispatchEvent(new PointerEvent('pointerdown', { button: 0, bubbles: true, clientX: 5, clientY: 5 })); return true; })()");
    report("clicking away dismisses the propose button", await waitFor(send,
      "document.querySelector('#annPropose').hidden", 5000));
    await evalValue(send, selectText);
    await waitFor(send, "!document.querySelector('#annPropose').hidden", 5000);
    await evalValue(send, "document.querySelector('#annPropose').click()");
    report("clicking propose stores the highlight (default color)", await waitFor(send,
      "window.__UI_TEST_SENT__.some(function (m) { return m.cmd === 'annotation_add'; })", 8000));
    report("storing does not open any dialog", (await evalValue(send, "document.querySelector('#annEditor').hidden")) === true);
    const annFirst = await evalValue(send, "window.__UI_TEST_SENT__.filter(function (m) { return m.cmd === 'annotation_add'; }).pop()");
    report("default highlight is yellow, note-free, verbatim quote",
      annFirst && annFirst.color === 'yellow' && annFirst.note === '' && annFirst.exact === 'alpha bravo charlie',
      JSON.stringify(annFirst));
    report("annotation auto-saves the unsaved document first", await waitFor(send,
      "window.__UI_TEST_SENT__.some(function (m) { return m.cmd === 'auto_save_document'; })", 8000));
    report("auto-save happens before the annotation is stored", await evalValue(send, `(() => {
      const cmds = window.__UI_TEST_SENT__.map(function (m) { return m.cmd; });
      const ai = cmds.lastIndexOf('annotation_add'), si = cmds.lastIndexOf('auto_save_document');
      return si !== -1 && ai !== -1 && si < ai;
    })()`));
    // Clicking the highlight reopens the collapsed palette; click-away dismisses it.
    await evalValue(send, clickOnHighlight);
    report("clicking the highlight opens the collapsed palette", await waitFor(send,
      "!document.querySelector('#annEditor').hidden && (document.querySelector('#annEditor .ann-note-wrap') || {}).hidden", 5000));
    await evalValue(send, "(() => { document.dispatchEvent(new MouseEvent('click', { clientX: 5, clientY: 5, bubbles: true })); return true; })()");
    report("clicking away dismisses the palette", await waitFor(send,
      "document.querySelector('#annEditor').hidden", 5000));
    await evalValue(send, clickOnHighlight);
    await waitFor(send, "!document.querySelector('#annEditor').hidden", 5000);
    await evalValue(send, "(() => { const b = document.querySelector('#annEditor .ann-color[data-color=\"blue\"]'); if (b) b.click(); return true; })()");
    report("switching color applies immediately", await waitFor(send,
      "window.__UI_TEST_SENT__.some(function (m) { return m.cmd === 'annotation_update' && m.color === 'blue'; })", 5000));
    // Pencil expands the note; typing saves itself, debounced — no button.
    await evalValue(send, clickOnHighlight);
    await waitFor(send, "!document.querySelector('#annEditor').hidden", 5000);
    await evalValue(send, "(() => { const b = document.querySelector('#annEditor .ann-pencil'); if (b) b.click(); return true; })()");
    report("pencil expands the note area", (await evalValue(send,
      "(document.querySelector('#annEditor .ann-note-wrap') || {}).hidden")) === false);
    await evalValue(send, "(() => { const ta = document.querySelector('#annEditor .ann-textarea'); ta.value = 'typed note'; ta.dispatchEvent(new Event('input', { bubbles: true })); return true; })()");
    report("typing auto-saves the note (no button)", await waitFor(send,
      "window.__UI_TEST_SENT__.some(function (m) { return m.cmd === 'annotation_update' && m.note === 'typed note'; })", 3000));
    report("a note shows a marker on the highlight", await waitFor(send,
      "!!document.querySelector('.ann-note-marker')", 5000));
    await evalValue(send, "document.querySelector('#annToggle').click()");
    report("badge counts the highlight", (await evalValue(send, "document.querySelector('#annCount').textContent")) === "1");
    report("panel lists it with the note", (await evalValue(send, "document.querySelectorAll('.ann-item').length")) === 1);
    await evalValue(send, "document.querySelector('#annToggle').click()"); // panel closed
    // Layering: a sub-chunk stacks inside the outer highlight; clicking the
    // overlapping region must open the INNER (most specific) annotation.
    const clickAtChar = (needle) => `(() => {
      const p = document.querySelector('.view.active .markdown-body p');
      const tn = p && p.firstChild;
      const ix = tn.nodeValue.indexOf(${JSON.stringify(needle)});
      const r = document.createRange();
      r.setStart(tn, ix);
      r.collapse(true);
      const rect = r.getBoundingClientRect();
      document.dispatchEvent(new MouseEvent('click', { clientX: rect.left + 2, clientY: rect.top + 2, bubbles: true }));
      return true;
    })()`;
    const selectBravo = `(() => {
      const p = document.querySelector('.view.active .markdown-body p');
      const tn = p && p.firstChild;
      const ix = tn ? tn.nodeValue.indexOf('bravo') : -1;
      if (ix < 0) return false;
      const r = document.createRange();
      r.setStart(tn, ix);
      r.setEnd(tn, ix + 5);
      const s = window.getSelection();
      s.removeAllRanges();
      s.addRange(r);
      document.dispatchEvent(new PointerEvent('pointerup', { button: 0, bubbles: true, clientX: 40, clientY: 40 }));
      return true;
    })()`;
    await evalValue(send, selectBravo);
    await waitFor(send, "!document.querySelector('#annPropose').hidden", 5000);
    await evalValue(send, clickAtChar('bravo')); // the click that ends the drag
    const dragRelease = await evalValue(send, "!document.querySelector('#annPropose').hidden && document.querySelector('#annEditor').hidden");
    report("clicking away after a selection keeps only the propose button", dragRelease === true);
    await evalValue(send, "document.querySelector('#annPropose').click()");
    report("proposing with a fresh selection deselects it (highlight must be visible)", await waitFor(send,
      "window.getSelection().rangeCount === 0 || window.getSelection().isCollapsed", 3000));
    report("a sub-chunk highlight is stored over the existing one", await waitFor(send, `(() => {
      const adds = window.__UI_TEST_SENT__.filter(function (m) { return m.cmd === 'annotation_add'; });
      return adds.length >= 2 && adds[adds.length - 1].exact === 'bravo';
    })()`, 8000));
    report("same-color layers stack in one paint group", await waitFor(send,
      "window.CSS && CSS.highlights && CSS.highlights.get('mdv-ann-blue').size >= 2", 8000));
    await evalValue(send, clickAtChar('bravo'));
    report("clicking the sub-chunk opens ITS editor (innermost wins)", await waitFor(send,
      "!document.querySelector('#annEditor').hidden && (document.querySelector('#annEditor .ann-note-wrap') || {}).hidden", 5000));
    await evalValue(send, "(() => { document.dispatchEvent(new MouseEvent('click', { clientX: 5, clientY: 5, bubbles: true })); return true; })()");
    await evalValue(send, clickAtChar('charlie'));
    report("clicking the noted outer highlight expands its note", await waitFor(send,
      "!document.querySelector('#annEditor').hidden && document.querySelector('#annEditor .ann-textarea').value === 'typed note'", 5000));
    await evalValue(send, "(() => { document.dispatchEvent(new MouseEvent('click', { clientX: 5, clientY: 5, bubbles: true })); return true; })()");

    // 7c. Responsiveness: the paint must land *before* the store write does.
    //     With the fake IPC slowed to 350ms, an optimistic highlight shows
    //     within milliseconds of the click.
    await evalValue(send, "window.__UI_TEST_SLOW_ADD__ = true;");
    await evalValue(send, selectBravo);
    await waitFor(send, "!document.querySelector('#annPropose').hidden", 5000);
    await evalValue(send, "document.querySelector('#annPropose').click()");
    await evalValue(send, "(() => new Promise(r => setTimeout(r, 80)))()");
    const optimistic = await evalValue(send, "window.CSS && CSS.highlights && CSS.highlights.get('mdv-ann-blue') ? CSS.highlights.get('mdv-ann-blue').size : 0");
    report("optimistic highlight paints before the store write lands", optimistic >= 3);
    report("the optimistic highlight settles after the store confirms", await waitFor(send,
      "document.querySelector('#annCount').textContent === '3'", 3000));
    await evalValue(send, clickAtChar('bravo'));
    await waitFor(send, "!document.querySelector('#annEditor').hidden", 5000);
    await evalValue(send, "(() => { const b = document.querySelector('#annEditor .ann-color[data-color=\"pink\"]'); if (b) b.click(); return true; })()");
    // The recolor hides the palette in its own handler; the same click must
    // NOT also reach the highlight underneath it and reopen its palette.
    report("recolor click does not leak through to the underlying highlight", (await evalValue(send,
      "document.querySelector('#annEditor').hidden")) === true);
    await evalValue(send, "(() => new Promise(r => setTimeout(r, 80)))()");
    const optimisticPink = await evalValue(send, "window.CSS && CSS.highlights && CSS.highlights.get('mdv-ann-pink') ? CSS.highlights.get('mdv-ann-pink').size : 0");
    report("optimistic recolor paints before the store write lands", optimisticPink >= 1);
    report("recolor settles after the store confirms", await waitFor(send,
      "window.__UI_TEST_SENT__.some(function (m) { return m.cmd === 'annotation_update' && m.color === 'pink'; })", 3000));
    await evalValue(send, "window.__UI_TEST_SLOW_ADD__ = false;");
    await waitFor(send, "document.querySelector('#annCount').textContent === '3'", 5000);
    // Three highlights now; the badge must end empty.
    const clickFirstItem = `(() => { const item = document.querySelector('.ann-item'); if (item) item.click(); return true; })()`;
    const clickTrash = `(() => { const b = document.querySelector('#annEditor .ann-trash'); if (b) b.click(); return true; })()`;
    const deleteOne = async () => {
      await evalValue(send, clickFirstItem);
      await evalValue(send, clickTrash);
      await evalValue(send, clickTrash);
    };
    await evalValue(send, "document.querySelector('#annToggle').click()");
    await evalValue(send, clickFirstItem);
    await evalValue(send, clickTrash);
    report("first click arms the icon delete", (await evalValue(send,
      "(document.querySelector('#annEditor .ann-trash') || {}).classList.contains('armed')")) === true);
    await evalValue(send, clickTrash);
    report("confirming fires annotation_delete", await waitFor(send,
      "window.__UI_TEST_SENT__.filter(function (m) { return m.cmd === 'annotation_delete'; }).length >= 1", 5000));
    await deleteOne();
    await deleteOne();
    report("deleting the last highlight clears the badge", await waitFor(send,
      "document.querySelector('#annCount').textContent === ''", 5000));
    await evalValue(send, "document.querySelector('#annToggle').click()"); // panel closed for the rest of the flow

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

    // 10. Anchor resolver: the edge cases that decide whether an annotation
    //     survives an edit — or is honestly marked needs review.
    const resolveCase = async (name, text, anchor, expected) => {
      const got = await evalValue(send,
        "window.mdviewAnnotationsResolve(" + JSON.stringify(text) + ", " + JSON.stringify(anchor) + ")");
      report(name, JSON.stringify(got) === JSON.stringify(expected), "got: " + JSON.stringify(got));
    };
    await resolveCase("resolver: unchanged position verifies", "hello brave world",
      { exact: "brave", prefix: "hello ", suffix: " world", start: 6, end: 11 }, { start: 6, end: 11 });
    report("resolver: inserted text before the quote re-anchors by content", await evalValue(send, `(() => {
      const t = "INSERTED\\nhello brave world";
      const s = t.indexOf("brave");
      const r = window.mdviewAnnotationsResolve(t, { exact: "brave", prefix: "hello ", suffix: " world", start: 6, end: 11 });
      return !!r && r.start === s && r.end === s + 5;
    })()`));
    await resolveCase("resolver: deleted passage orphans instead of guessing", "hello world",
      { exact: "brave", prefix: "hello ", suffix: " world", start: 6, end: 11 }, null);
    await resolveCase("resolver: duplicated passage with distinguishing context resolves to the right one", "aa BB cc BB dd",
      { exact: "BB", prefix: "aa BB cc ", suffix: " dd", start: 0, end: 2 }, { start: 9, end: 11 });
    await resolveCase("resolver: stale duplicate with identical context refuses to guess", "x BB y x BB y",
      { exact: "BB", prefix: "x ", suffix: " y", start: 5, end: 7 }, null);
    await resolveCase("resolver: unique quote re-anchors even with zero context match", "preface brave epilogue",
      { exact: "brave", prefix: "zzz", suffix: "zzz", start: 3, end: 8 }, { start: 8, end: 13 });
    await resolveCase("resolver: emoji offsets stay consistent", "a\u{1F600}b",
      { exact: "\u{1F600}b", prefix: "a", suffix: "", start: 1, end: 4 }, { start: 1, end: 4 });
    await resolveCase("resolver: quote at the very start of the document", "first line",
      { exact: "first", prefix: "", suffix: " line", start: 0, end: 5 }, { start: 0, end: 5 });
    await resolveCase("resolver: quote at the very end of the document", "one two three",
      { exact: "three", prefix: "two ", suffix: "", start: 8, end: 13 }, { start: 8, end: 13 });
    await resolveCase("resolver: empty selection is unusable", "abc",
      { exact: "   ", prefix: "", suffix: "", start: 0, end: 3 }, null);

    // 9. Bridge contract: every request id is the bridge's own string — the
    //    exact regression that hung every DB call.
    const ids = await evalValue(send, "window.__UI_TEST_SENT__.map(function (m) { return m.id; })");
    report("every request id is the bridge's own (rN)",
      Array.isArray(ids) && ids.length > 0 && ids.every((id) => typeof id === "string" && /^r\d+$/.test(id)),
      JSON.stringify(ids));

    // 10. Delete a saved document from the home screen. The annotation flow
    //     auto-saved a second document earlier in this suite, so the list now
    //     holds two rows — target the "Renamed doc" row by title.
    const clickRowDelete = `(() => {
      const rows = Array.from(document.querySelectorAll('.saved-item'));
      const row = rows.find(r => r.querySelector('.saved-title') && r.querySelector('.saved-title').textContent === 'Renamed doc');
      const x = row && row.querySelector('.saved-x');
      if (x) x.click();
      return true;
    })()`;
    report("home lists the saved document with a delete button", await waitFor(send,
      "document.querySelectorAll('.saved-item .saved-x').length >= 2", 5000));
    await evalValue(send, clickRowDelete);
    report("first click arms the confirm", await evalValue(send, `(() => {
      const rows = Array.from(document.querySelectorAll('.saved-item'));
      const row = rows.find(r => r.querySelector('.saved-title') && r.querySelector('.saved-title').textContent === 'Renamed doc');
      const x = row && row.querySelector('.saved-x');
      return x && x.textContent;
    })()`) === "Delete?");
    await evalValue(send, clickRowDelete);
    report("second click deletes the row", await waitFor(send,
      "!Array.from(document.querySelectorAll('.saved-title')).some(function (e) { return e.textContent === 'Renamed doc'; })", 5000));
    report("the other saved document is untouched",
      (await evalValue(send, "document.querySelectorAll('.saved-item').length")) === 1);
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
