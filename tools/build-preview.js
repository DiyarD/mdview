// Assemble C:\dev\mdview\preview-theme.html from the real frontend sources,
// mirroring exactly what Rust build_html() injects.
const fs = require("fs");
const zlib = require("zlib");
const root = "C:\\dev\\mdview";
let html = fs.readFileSync(root + "\\frontend\\index.html", "utf8");
const light = fs.readFileSync(root + "\\frontend\\vendor\\hljs-light.css", "utf8");
let dark = fs.readFileSync(root + "\\frontend\\vendor\\hljs-dark.css", "utf8");
// hljs ships gzipped (Rust inflates it at boot); the preview needs the source.
const js = zlib.gunzipSync(fs.readFileSync(root + "\\frontend\\vendor\\hljs.min.js.gz")).toString("utf8");
// scope_css mirror (must match src/main.rs)
function scopeCss(css, scope) {
  let out = "";
  for (const chunk of css.split("}")) {
    const pos = chunk.indexOf("{");
    if (pos !== -1) {
      const sel = chunk.slice(0, pos), decl = chunk.slice(pos);
      out += sel.split(",").map(s => scope + " " + s.trim()).join(",") + decl + "}";
    } else out += chunk;
  }
  return out;
}
dark = scopeCss(dark, "html.dark");
html = html.split("__MDVIEW_HLJS_LIGHT_CSS__").join(light);
html = html.split("__MDVIEW_HLJS_DARK_CSS__").join(dark);
html = html.split("__MDVIEW_HLJS_JS__").join(js.split("</script").join("<\\/script"));
html = html.split("__MDVIEW_INITIAL_JSON__").join('{"name":"","text":"","base":""}');
html = html.split("__MDVIEW_PREFS_JSON__").join('{"theme":null,"fontScale":1.0,"readWidth":860}');
fs.writeFileSync(root + "\\preview-theme.html", html);
console.log("preview bytes:", html.length);
