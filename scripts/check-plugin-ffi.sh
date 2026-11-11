#!/usr/bin/env bash
#
# Plugin FFI `unsafe` lives only in the per-format `-ffi` crates; the host
# crates and the code that drives them forbid it (design 016 §2.9).
#
# This gate fails if, for CLAP, VST3, AU and VST2 (`<fmt>` below is each):
#
# - any `unsafe` token appears in `tutti-<fmt>-host` (the whole of `src/`,
#   tests inside it included: the crate is `#![forbid(unsafe_code)]`);
# - `tutti-<fmt>-host`'s `lib.rs` lacks `#![forbid(unsafe_code)]`;
# - any `unsafe` token appears in `tutti-plugin-server/src/` or its `lib.rs`
#   lacks `#![forbid(unsafe_code)]`: the server is one generic shell over the
#   formats' `Loaded` (design 021 step 5) and has no `unsafe` of its own, test
#   modules included.
# - `tutti-<fmt>-ffi`'s `lib.rs` lacks the lints that make every `unsafe` block
#   carry a `// SAFETY:` comment (`undocumented_unsafe_blocks`) and every
#   `unsafe fn` a `# Safety` section (`missing_safety_doc`).
#
# VST2's FFI is `tutti-vst2-ffi` (the host half of the vst-rs fork; the plugin-authoring half
# is the test-only `testing/tutti-vst2-author`). Besides the rules above:
#
# - outside `tutti-vst2-ffi` and `tutti-vst2-host`, `unsafe` before the test module of a file
#   whose path has `vst2` in it or that names `tutti_vst2_host` or `tutti_vst2_ffi` is pinned
#   to the known caller sites (`VST2_CALLERS`, none now: the in-process node is generic over
#   the format and names no VST2 type, and a parent window is built safely,
#   `WindowHandle::from_window`). A new one fails here, and so does a removed one
#   until its pin is lowered.
#
# Comments and string literals are not code: prose that says "unsafe" is fine.
#
# Mutation (run): add an `unsafe { }` block to
# `tutti-plugin/src/host/plugin.rs` (it names `Vst2Loaded`), or delete a `deny` lint
# from `tutti-vst2-ffi`'s or `tutti-vst2-host`'s `lib.rs` -> this script exits 1
# naming the file. Also: add `unsafe { }` to any fn of `tutti-clap-host/src/lib.rs`'s
# or `tutti-vst3-host/src/lib.rs`'s crate, or delete its forbid attribute, or add an `unsafe` block above the
# file of `tutti-plugin-server/src/` -> this script exits 1 naming the file.
#
# No build needed (python3 only), so CI runs it in the static `paths` job.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import glob, re, sys

# (format name, host crate, FFI crate).
FORMATS = [
    ("CLAP", "crates/tutti-clap-host", "crates/tutti-clap-ffi"),
    ("VST3", "crates/tutti-vst3-host", "crates/tutti-vst3-ffi"),
    ("AU", "crates/tutti-au-host", "crates/tutti-au-ffi"),
    ("VST2", "crates/tutti-vst2-host", "crates/tutti-vst2-ffi"),
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


# VST2: the crates that hold its FFI and its host, and the callers outside them that may hold
# `unsafe` (file -> the number of `unsafe` tokens in code before the file's test module).
VST2_CRATES = ["crates/tutti-vst2-ffi", "crates/tutti-vst2-host"]
VST2_CALLERS = {}


def code_before_tests(path):
    s = open(path).read()
    t = re.search(r"^\s*#\[cfg\((all\()?test\b[^\n]*\]\s*\n(\s*#\[[^\n]*\]\s*\n)*\s*(pub )?mod \w+", s, re.M)
    return code_only(s[: t.start()] if t else s)


bad, scanned = [], 0
for path in sorted(glob.glob("crates/*/src/**/*.rs", recursive=True)):
    if any(path.startswith(c + "/") for c in VST2_CRATES):
        continue
    code = code_before_tests(path)
    if "vst2" not in path and not re.search(r"\b(tutti_vst2_host|tutti_vst2_ffi|Vst2)", code):
        continue
    scanned += 1
    n = len(re.findall(r"\bunsafe\b", code))
    want = VST2_CALLERS.get(path, 0)
    if n != want:
        bad.append(
            f"{path}: {n} `unsafe` before the test module, pinned at {want} "
            f"(VST2 unsafe lives in tutti-vst2-ffi)"
        )
for path in VST2_CALLERS:
    if not glob.glob(path):
        bad.append(f"{path}: pinned VST2 caller does not exist; was it moved?")

for name, HOST, FFI in FORMATS:
    host_files = sorted(glob.glob(f"{HOST}/src/**/*.rs", recursive=True))
    if not host_files:
        sys.exit(f"check-plugin-ffi: {HOST}/src matches no file; was the crate moved?")
    for path in host_files:
        scanned += 1
        line = first_unsafe(path, False)
        if line:
            bad.append(f"{path}:{line}: `unsafe` in {HOST} (it forbids unsafe_code)")
    if not re.search(r"#!\[forbid\(unsafe_code\)\]", attrs(f"{HOST}/src/lib.rs")):
        bad.append(f"{HOST}/src/lib.rs: missing #![forbid(unsafe_code)]")
    ffi_lib = attrs(f"{FFI}/src/lib.rs")
    for lint in ("undocumented_unsafe_blocks", "missing_safety_doc"):
        if not re.search(r"#!\[deny\([^\]]*" + lint, ffi_lib):
            bad.append(f"{FFI}/src/lib.rs: missing #![deny(clippy::{lint})]")

# The server drives every served format through `Loaded` and owns no `unsafe`.
SERVER = "crates/tutti-plugin-server"
server_files = sorted(glob.glob(f"{SERVER}/src/**/*.rs", recursive=True))
if not server_files:
    sys.exit(f"check-plugin-ffi: {SERVER}/src matches no file; was the crate moved?")
for path in server_files:
    scanned += 1
    line = first_unsafe(path, False)
    if line:
        bad.append(f"{path}:{line}: `unsafe` in {SERVER} (it forbids unsafe_code)")
if not re.search(r"#!\[forbid\(unsafe_code\)\]", attrs(f"{SERVER}/src/lib.rs")):
    bad.append(f"{SERVER}/src/lib.rs: missing #![forbid(unsafe_code)]")

if scanned < 40:
    sys.exit(f"check-plugin-ffi: only {scanned} files scanned; the scan is broken")
for b in bad:
    print(b, file=sys.stderr)
if bad:
    sys.exit(1)
print(f"check-plugin-ffi: {scanned} files, no CLAP, VST3, AU or VST2 unsafe outside the -ffi crates, VST2 callers held to their pins")
PY
