#!/usr/bin/env python3
"""Compress the compressible vendored assets in place (gzip level 9).

Mermaid/KaTeX/hljs are stored gzipped inside the binary and inflated by Rust
on demand (see gunzip() in src/main.rs). Fonts (.woff2) are already
deflate-resistant and are left alone — compressing them makes them bigger.
Run after updating any vendor file, then rebuild.
"""
import gzip
import os
import sys

VENDOR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frontend", "vendor")
VENDOR = os.path.normpath(VENDOR)

# (relative path, enabled) — .woff2 deliberately excluded.
TARGETS = [
    ("mermaid/mermaid.min.js", True),
    ("katex/katex.min.js", True),
    ("katex/katex.min.css", True),
    ("hljs.min.js", True),
]


def main() -> int:
    for rel, enabled in TARGETS:
        src = os.path.join(VENDOR, rel)
        gz = src + ".gz"
        if not os.path.isfile(src):
            if os.path.isfile(gz):
                print("skip (already compressed):", rel)
                continue
            print("MISSING:", rel, file=sys.stderr)
            return 1
        if not enabled:
            continue
        with open(src, "rb") as f:
            data = f.read()
        blob = gzip.compress(data, 9)
        with open(gz, "wb") as f:
            f.write(blob)
        os.remove(src)  # the .gz is now the single source of truth
        print("%-28s %8d -> %7d B (%4.1f%%)" % (rel, len(data), len(blob), 100.0 * len(blob) / len(data)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
