#!/usr/bin/env bash
#
# MIDI OS `unsafe` lives only in the two `-ffi` crates; the backend above them
# forbids it (design 017 stage 4d; the model is `check-plugin-ffi.sh`).
#
# This gate fails if:
#
# - any `unsafe` token appears in `tutti-midi-hardware` (the whole of `src/` plus
#   `build.rs`, `tests/`, `examples/` and `benches/`: the crate is
#   `#![forbid(unsafe_code)]`, so a test cannot carry one either);
# - `tutti-midi-hardware`'s `lib.rs` lacks `#![forbid(unsafe_code)]`;
# - `tutti-midi-hardware`'s `Cargo.toml` names a crate that only unsafe code
#   needs (`libc`, `coremidi-sys`, `mach2`, `tutti-alsa-ump-sys`): the raw
#   bindings are the `-ffi` crates' dependencies, not the backend's;
# - `tutti-alsa-ffi`'s or `tutti-coremidi-ffi`'s `lib.rs` lacks the lints that make
#   every `unsafe` block carry a `// SAFETY:` comment
#   (`undocumented_unsafe_blocks`) and every `unsafe fn` a `# Safety` section
#   (`missing_safety_doc`).
#
# Comments and string literals are not code: prose that says "unsafe" is fine.
#
# Mutation (run): add an `unsafe { }` block to any file of
# `tutti-midi-hardware/src` (`backend/alsa/poll.rs`, `backend/coremidi/mod.rs`, ...),
# or to its `tests/`; delete its `#![forbid(unsafe_code)]`; add `libc = "0.2"` to
# its Cargo.toml; delete the `missing_safety_doc` lint from `tutti-alsa-ffi`'s or
# `tutti-coremidi-ffi`'s `lib.rs` -> this script exits 1 naming the file.
#
# No build needed (python3 only), so CI runs it in the static `paths` job.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import glob, re, sys

HOST = "crates/tutti-midi-hardware"
FFI = ["crates/tutti-alsa-ffi", "crates/tutti-coremidi-ffi"]
# Dependencies that exist only to make unsafe calls: they belong to the -ffi crates.
RAW_DEPS = ["libc", "coremidi-sys", "mach2", "tutti-alsa-ump-sys"]


def code_only(s):
    """`s` without comments and string/char literals (their text blanked)."""
    out, i, n = [], 0, len(s)
    while i < n:
        if s.startswith("//", i):
            j = s.find("\n", i)
            i = n if j < 0 else j
        elif s.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if s.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif s.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        elif s[i] == '"':
            i += 1
            while i < n and s[i] != '"':
                i += 2 if s[i] == "\\" else 1
            i += 1
            out.append('""')
        elif s[i] == "r" and re.match(r'r#*"', s[i:]):
            hashes = len(re.match(r"r(#*)", s[i:]).group(1))
            end = s.find('"' + "#" * hashes, i + 2 + hashes)
            i = n if end < 0 else end + 1 + hashes
            out.append('""')
        elif s[i] == "'":
            m = re.match(r"'(\\.[^']*|[^\\'])'", s[i:])
            if m:
                i += m.end()
                out.append("''")
            else:
                out.append(s[i])
                i += 1
        else:
            out.append(s[i])
            i += 1
    return "".join(out)


bad, scanned = [], 0

files = sorted(
    f
    for pattern in ("src/**/*.rs", "tests/**/*.rs", "examples/**/*.rs", "benches/**/*.rs", "build.rs")
    for f in glob.glob(f"{HOST}/{pattern}", recursive=True)
)
if not any(f.startswith(f"{HOST}/src/") for f in files):
    sys.exit(f"check-midi-ffi: {HOST}/src matches no file; was the crate moved?")
for path in files:
    scanned += 1
    code = code_only(open(path).read())
    m = re.search(r"\bunsafe\b", code)
    if m:
        line = code.count("\n", 0, m.start()) + 1
        bad.append(f"{path}:{line}: `unsafe` in {HOST} (it forbids unsafe_code)")

if not re.search(r"#!\[forbid\(unsafe_code\)\]", code_only(open(f"{HOST}/src/lib.rs").read())):
    bad.append(f"{HOST}/src/lib.rs: missing #![forbid(unsafe_code)]")

manifest = open(f"{HOST}/Cargo.toml").read()
for dep in RAW_DEPS:
    if re.search(r"^\s*" + re.escape(dep) + r"\s*(=|\.)", manifest, re.M):
        bad.append(f"{HOST}/Cargo.toml: depends on `{dep}` (raw FFI belongs to the -ffi crates)")

for ffi in FFI:
    scanned += 1
    lib = code_only(open(f"{ffi}/src/lib.rs").read())
    for lint in ("undocumented_unsafe_blocks", "missing_safety_doc"):
        if not re.search(r"#!\[deny\([^\]]*" + lint, lib):
            bad.append(f"{ffi}/src/lib.rs: missing #![deny(clippy::{lint})]")

if scanned < 20:
    sys.exit(f"check-midi-ffi: only {scanned} files scanned; the scan is broken")
for b in bad:
    print(b, file=sys.stderr)
if bad:
    sys.exit(1)
print(f"check-midi-ffi: {scanned} files, no ALSA or CoreMIDI unsafe outside the -ffi crates")
PY
