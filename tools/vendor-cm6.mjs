// Build the vendored CodeMirror 6 bundle: frontend/vendor/codemirror.min.js
// (+ .gz, gzip level 9, same convention as tools/vendor-assets.py).
//
// Usage: node tools/vendor-cm6.mjs
//
// Installs pinned official-registry packages into a scratch prefix
// (tools/.cm-build/, gitignored), bundles tools/vendor-cm6-entry.mjs with
// esbuild, and writes the minified IIFE bundle plus its gzip twin.
// No network access beyond the npm registry.
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PREFIX = path.join(ROOT, 'tools', '.cm-build');
const ENTRY = path.join(ROOT, 'tools', 'vendor-cm6-entry.mjs');
const OUT = path.join(ROOT, 'frontend', 'vendor', 'codemirror.min.js');

// Pinned to the exact latest 6.x (1.x for @lezer/highlight) versions resolved
// from the npm registry on 2026-11-15 via `npm view <pkg> version`.
const PINNED = {
	'codemirror': '6.0.2',
	'@codemirror/view': '6.43.15',
	'@codemirror/state': '6.7.6',
	'@codemirror/commands': '6.11.1',
	'@codemirror/language': '6.13.1',
	'@codemirror/lang-markdown': '6.5.2',
	'@lezer/highlight': '1.2.5',
};
const ESBUILD_VERSION = '0.28.2';

// npm's JS entry point, resolved relative to this node binary. Running it via
// node avoids spawning .cmd shims (spawnSync of a .cmd raises EINVAL) and PATH lookups.
const NPM_CLI = process.platform === 'win32'
	? path.join(path.dirname(process.execPath), 'node_modules', 'npm', 'bin', 'npm-cli.js')
	: null;
const NPM_CMD = process.platform === 'win32' ? process.execPath : 'npm';
const NPM_ARGS = process.platform === 'win32' ? [NPM_CLI] : [];

function run(cmd, args) {
	console.log('>', cmd, ...args);
	const r = spawnSync(cmd, args, { stdio: 'inherit', cwd: ROOT });
	if (r.status !== 0) {
		throw new Error(`${cmd} exited with ${r.status ?? r.error}`);
	}
}

function pkgVersion(name) {
	const p = path.join(PREFIX, 'node_modules', ...name.split('/'), 'package.json');
	return JSON.parse(fs.readFileSync(p, 'utf8')).version;
}

// Clean scratch dir for a deterministic resolve.
fs.rmSync(PREFIX, { recursive: true, force: true });
fs.mkdirSync(PREFIX, { recursive: true });

// Install the pinned runtime deps (+ esbuild as a scratch build tool).
const installArgs = ['install', '--no-save', '--no-audit', '--no-fund', '--loglevel=error', '--prefix', PREFIX];
for (const [name, version] of Object.entries(PINNED)) installArgs.push(`${name}@${version}`);
installArgs.push(`esbuild@${ESBUILD_VERSION}`);
run(NPM_CMD, [...NPM_ARGS, ...installArgs]);

// Resolved versions (authoritative: what actually got installed).
console.log('\nResolved versions:');
const resolved = {};
for (const name of Object.keys(PINNED)) {
	resolved[name] = pkgVersion(name);
	console.log(`  ${name}@${resolved[name]}`);
}
const esbuildVersion = pkgVersion('esbuild');
console.log(`  esbuild@${esbuildVersion}`);

// Bundle with the locally installed esbuild (invoked via its JS bin, no PATH lookups).
// NODE_PATH points at the scratch prefix's node_modules so esbuild can resolve
// the entry file's imports from there (entry lives outside the prefix).
const esbuildBin = path.join(PREFIX, 'node_modules', 'esbuild', 'bin', 'esbuild');
const outDir = path.dirname(OUT);
fs.mkdirSync(outDir, { recursive: true });
const esbuildArgs = [
	esbuildBin,
	ENTRY,
	'--bundle',
	'--minify',
	'--format=iife',
	'--global-name=MDViewCM',
	'--target=es2020',
	'--legal-comments=inline',
	'--banner:js=/* CodeMirror 6 (MIT) — (c) Marijn Haverbeke and contributors. Bundled from @codemirror/* 6.x; see tools/vendor-cm6.mjs */',
	`--outfile=${OUT}`,
];
console.log('>', process.execPath, ...esbuildArgs);
const es = spawnSync(process.execPath, esbuildArgs, {
	stdio: 'inherit',
	cwd: ROOT,
	env: { ...process.env, NODE_PATH: path.join(PREFIX, 'node_modules') },
});
if (es.status !== 0) {
	throw new Error(`esbuild exited with ${es.status ?? es.error}`);
}

// Gzip twin, level 9 (same convention as tools/vendor-assets.py).
const raw = fs.readFileSync(OUT);
const gz = zlib.gzipSync(raw, { level: 9 });
fs.writeFileSync(OUT + '.gz', gz);

console.log('\nOutput:');
console.log(`  ${path.relative(ROOT, OUT)}  ${raw.length} B (raw)`);
console.log(`  ${path.relative(ROOT, OUT + '.gz')}  ${gz.length} B (gzip -9)`);

// Sanity check: the bundle must define the MDViewCM globals.
const src = raw.toString('utf8');
for (const n of ['EditorView', 'history', 'defaultKeymap', 'markdownLanguage']) {
	console.log(`  global ${n}: ${src.includes(n)}`);
}
