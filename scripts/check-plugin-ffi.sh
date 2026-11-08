#!/usr/bin/env bash
#
# Plugin FFI `unsafe` lives only in the per-format `-ffi` crates; the host
# crates and the code that drives them forbid it (design 016 §2.9).
#
# This gate fails if, for CLAP, VST3 and AU (`<fmt>` below is each):
#
# - any `unsafe` token appears in `tutti-<fmt>-host` (the whole of `src/`,
#   tests inside it included: the crate is `#![forbid(unsafe_code)]`);
# - `tutti-<fmt>-host`'s `lib.rs` lacks `#![forbid(unsafe_code)]`;
# - any `unsafe` token appears in `tutti-plugin-server/src/loaders/<fmt>.rs` or
#   `tutti-plugin/src/format/gui/<fmt>.rs` before their `#[cfg(test)]` module.
#   Their test modules may call into the reference plugin's cdylib through
#   `libloading`, which is `unsafe` and cannot go through the FFI crate, so the
#   scan stops where the test module (`#[cfg(test)]` then `mod`) starts.
# - `tutti-<fmt>-ffi`'s `lib.rs` lacks the lints that make every `unsafe` block
#   carry a `// SAFETY:` comment (`undocumented_unsafe_blocks`) and every
#   `unsafe fn` a `# Safety` section (`missing_safety_doc`).
#
# Comments and string literals are not code: prose that says "unsafe" is fine.
#
# Mutation (run): add `unsafe { }` to any fn of `tutti-clap-host/src/lib.rs`'s
# or `tutti-vst3-host/src/lib.rs`'s crate, or delete its forbid attribute, or add an `unsafe` block above the
# test module of `loaders/clap.rs`, even after an earlier `#[cfg(test)] use`
# -> this script exits 1 naming the file.
#
# No build needed (python3 only), so CI runs it in the static `paths` job.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import glob, re, sys

# (format name, host crate, FFI crate, files the host's code drives the FFI from:
# no `unsafe` before their first `#[cfg(test)]` item).
FORMATS = [
    (
        "CLAP",
        "crates/tutti-clap-host",
        "crates/tutti-clap-ffi",
        [
            "crates/tutti-plugin-server/src/loaders/clap.rs",
            "crates/tutti-plugin/src/format/gui/clap.rs",
        ],
    ),
    (
        "VST3",
        "crates/tutti-vst3-host",
        "crates/tutti-vst3-ffi",
        [
            "crates/tutti-plugin-server/src/loaders/vst3.rs",
            "crates/tutti-plugin/src/format/gui/vst3.rs",
        ],
    ),
    # The AU loaders still carry two `unsafe` sites of their own
    # (`WindowHandle::from_raw`, doc 016 step 5), so only the host and the FFI
    # crate are held here.
    ("AU", "crates/tutti-au-host", "crates/tutti-au-ffi", []),
]


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


def first_unsafe(path, stop_at_tests):
    s = open(path).read()
    if stop_at_tests:
        # The test *module*: a `#[cfg(test)] use` or fn above production code
        # does not end the scan.
        t = re.search(r"^\s*#\[cfg\(test\)\]\s*\n(\s*#\[[^\n]*\]\s*\n)*\s*(pub )?mod \w+", s, re.M)
        s = s[: t.start()] if t else s
    code = code_only(s)
    m = re.search(r"\bunsafe\b", code)
    if m:
        return code.count("\n", 0, m.start()) + 1
    return None


def attrs(path):
    return code_only(open(path).read())


bad, scanned = [], 0
for name, HOST, FFI, OUTSIDE in FORMATS:
    host_files = sorted(glob.glob(f"{HOST}/src/**/*.rs", recursive=True))
    if not host_files:
        sys.exit(f"check-plugin-ffi: {HOST}/src matches no file; was the crate moved?")
    for path in host_files:
        scanned += 1
        line = first_unsafe(path, False)
        if line:
            bad.append(f"{path}:{line}: `unsafe` in {HOST} (it forbids unsafe_code)")
    for path in OUTSIDE:
        scanned += 1
        line = first_unsafe(path, True)
        if line:
            bad.append(
                f"{path}:{line}: `unsafe` outside the test module ({name} unsafe lives in {FFI})"
            )
    if not re.search(r"#!\[forbid\(unsafe_code\)\]", attrs(f"{HOST}/src/lib.rs")):
        bad.append(f"{HOST}/src/lib.rs: missing #![forbid(unsafe_code)]")
    ffi_lib = attrs(f"{FFI}/src/lib.rs")
    for lint in ("undocumented_unsafe_blocks", "missing_safety_doc"):
        if not re.search(r"#!\[deny\([^\]]*" + lint, ffi_lib):
            bad.append(f"{FFI}/src/lib.rs: missing #![deny(clippy::{lint})]")

if scanned < 40:
    sys.exit(f"check-plugin-ffi: only {scanned} files scanned; the scan is broken")
for b in bad:
    print(b, file=sys.stderr)
if bad:
    sys.exit(1)
print(f"check-plugin-ffi: {scanned} files, no CLAP, VST3 or AU unsafe outside the -ffi crates")
PY
