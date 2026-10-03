const fs = require("fs");
const { execSync } = require("child_process");
const src = fs.readFileSync("frontend/index.html", "utf8");
// Extract the main inline script (the one with "use strict").
const m = src.match(/<script>\n"use strict";([\s\S]*?)<\/script>/);
if (!m) { console.error("main script not found"); process.exit(1); }
fs.writeFileSync("C:\\Users\\TOTALT~1\\AppData\\Local\\Temp\\opencode\\mdview-main.js", '"use strict";' + m[1]);
try {
  execSync('node --check "C:\\Users\\TOTALT~1\\AppData\\Local\\Temp\\opencode\\mdview-main.js"', { stdio: "pipe" });
  console.log("SYNTAX OK, script bytes:", m[1].length);
} catch (e) {
  console.error("SYNTAX FAIL:\n" + (e.stdout || "") + (e.stderr || "") + e.message);
  process.exit(1);
}
