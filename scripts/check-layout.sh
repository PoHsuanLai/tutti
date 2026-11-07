#!/usr/bin/env bash
#
# The repo layout and the crate layering, from `cargo metadata`.
#
# Layout:
# - every workspace member under `crates/` is directly `crates/<package-name>`;
#   a nested directory (`crates/foo/bar`) is refused, and so is a directory
#   under `crates/` that is not a member;
# - every workspace member under `testing/` is directly `testing/<dir>`;
# - no member lives anywhere else, and `vendor/` holds no member;
# - nothing under `crates/` has a normal or build dependency on anything under
#   `testing/` (a dev-dependency is fine).
#
# Layering (CLAUDE.md, "Workspace"): a normal or build dependency between
# workspace crates points strictly down the LAYERS table. A crate missing from
# the table is an error: placing it is the point. On top of that:
# - `arc-lend` and `tutti-ump` depend on no workspace crate, of any kind;
# - `tutti-types` depends on no workspace crate, and `tutti-rt` on `arc-lend`
#   alone;
# - `tutti-dsp` and `tutti-mod` depend on `tutti-types` alone,
#   `tutti-midi-types` on `tutti-types`, `tutti-rt` and `tutti-ump`, and
#   `tutti-graph` on those three;
# - `tutti-midi-protocol` depends on `tutti-ump` alone;
# - `tutti-spatial` depends on `tutti-nodes`, and `tutti-nodes` never on
#   `tutti-spatial`, of any kind;
# - nothing but `tutti` and `bevy-tutti` depends on `tutti-session`, and
#   nothing depends on `tutti` or `bevy-tutti`.
#
# Mutation record (each planted on the tree that added this script, seen to
# exit 1, then removed):
# - `tutti-nodes` as a `[dependencies]` entry of `tutti-types`: "tutti-types ->
#   tutti-nodes (normal) points up" and "tutti-types may depend on no workspace
#   crate"; `arc-lend` as a `[dependencies]` entry of `tutti-types`: "tutti-types
#   may depend on no workspace crate"; `tutti-types` as a `[dependencies]` entry
#   of `tutti-rt`: "tutti-rt may depend on ['arc-lend'] only";
# - `tutti-spatial` as a `[dev-dependencies]` entry of `tutti-nodes`:
#   "tutti-nodes must not depend on tutti-spatial";
# - `tutti-fixtures` as a `[dependencies]` entry of `tutti-dsp`, and as
#   a `[build-dependencies]` entry of `tutti-plugin`: "normal dependency on
#   tutti-fixtures; crates/ depends on testing/" and "build dependency
#   ...";
# - `tutti-mod` moved to `crates/nest/tutti-mod`: "is a nested directory under
#   crates/" (a nested directory under crates/ is refused);
# - `tutti-mod` moved to `crates/modx`: "must be crates/tutti-mod";
# - `mkdir crates/stray`: "crates/stray is not a workspace member";
# - `tutti-graph` deleted from LAYERS: "tutti-graph is not in LAYERS";
# - `tutti-types` as a `[dependencies]` entry of `tutti-ump`: "tutti-ump -> tutti-types
#   (normal) points up" and "tutti-ump may depend on no workspace crate";
# - `tutti-core` as a `[dependencies]` entry of `tutti-graph`: "tutti-graph may
#   depend on ['tutti-rt', 'tutti-types', 'tutti-ump'] only";
# - `tutti-types` as a `[dependencies]` entry of `tutti-midi-protocol`:
#   "tutti-midi-protocol may depend on ['tutti-ump'] only";
# - `tutti-midi-protocol` deleted from LAYERS: "tutti-midi-protocol is not in
#   LAYERS".
#
# - a test finds a repo file through `tutti_fixtures`, never through a path
#   that depends on its own depth. In a `.rs` file under `crates/` or `testing/`
#   (build scripts and EXEMPT_FILES excepted; `include_str!`, `include!`,
#   `include_bytes!` and `#[path]` are source-relative and ignored) it denies a
#   string literal with `..` as a path component when the file mentions
#   `CARGO_MANIFEST_DIR` or the literal is an argument of `Path::new`,
#   `PathBuf::from`, `.join`, `.push` or `concat!`; a literal climbing to
#   `assets`; and `.ancestors()` or `.parent()` in a file that mentions
#   `CARGO_MANIFEST_DIR`.
#   Limits: a textual scan. A `..` built at run time (`format!`, a
#   non-literal) or a literal passed through a helper in a file that does not
#   mention `CARGO_MANIFEST_DIR` is not seen.
#   Mutation (each planted at the end of `tutti-dsp/src/lib.rs`, seen to exit
#   1, removed): `concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/x")`;
#   `let m = env!("CARGO_MANIFEST_DIR"); Path::new(m).join("../../fixtures/x")`;
#   `Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2)`;
#   `Path::new("../../samples/x.sf2")`; `.join("..").join("..").join("assets")`.
#
# Needs a Rust toolchain (cargo metadata) and python3. No build.
set -euo pipefail
cd "$(dirname "$0")/.."

meta_file=$(mktemp)
trap 'rm -f "$meta_file"' EXIT
cargo metadata --locked --no-deps --format-version 1 > "$meta_file"

python3 -I - "$meta_file" <<'PY'
import json, os, sys

with open(sys.argv[1]) as f:
    meta = json.load(f)
root = meta["workspace_root"]
pkgs = {p["name"]: p for p in meta["packages"]}

def rel(p):
    return os.path.relpath(os.path.dirname(p["manifest_path"]), root)

# Lowest first. A normal or build dependency must land in a strictly lower
# layer. Derived from the real edges; see CLAUDE.md.
LAYERS = [
    # no workspace dependency at all
    ["arc-lend", "vst-tutti", "tutti-alsa-ump-sys", "tutti-rustysynth", "tutti-ump"],
    ["tutti-types", "tutti-rt", "tutti-midi-protocol"],
    ["tutti-graph", "tutti-dsp", "tutti-mod", "tutti-midi-types"],
    ["tutti-core", "tutti-plugin-types"],
    ["tutti-io", "tutti-nodes", "tutti-midi-runtime", "tutti-midi-file",
     "tutti-clap-ffi", "tutti-vst3-ffi", "tutti-au-host", "tutti-vst2-host"],
    ["tutti-cpal", "tutti-export", "tutti-spatial", "tutti-sampler",
     "tutti-polysynth", "tutti-soundfont", "tutti-midi-hardware",
     "tutti-clap-host", "tutti-vst3-host"],
    ["tutti-plugin"],
    ["tutti-plugin-server", "tutti-session"],
    ["tutti", "bevy-tutti"],
]
layer = {n: i for i, ns in enumerate(LAYERS) for n in ns}

errors = []
def fail(msg):
    errors.append(msg)

# ---- layout ---------------------------------------------------------------
member_dirs = set()
for name, p in pkgs.items():
    d = rel(p)
    member_dirs.add(d)
    parts = d.split("/")
    if parts[0] == "crates":
        if len(parts) != 2:
            fail(f"{name}: {d} is a nested directory under crates/; members sit directly in crates/")
        elif parts[1] != name:
            fail(f"{name}: {d} must be crates/{name}")
    elif parts[0] == "testing":
        if len(parts) != 2:
            fail(f"{name}: {d} is a nested directory under testing/; members sit directly in testing/")
    else:
        fail(f"{name}: {d} is outside crates/ and testing/ (vendor/ holds third-party code, never a member)")

for top in ("crates", "testing"):
    for entry in sorted(os.listdir(os.path.join(root, top))):
        path = os.path.join(root, top, entry)
        if os.path.isdir(path) and f"{top}/{entry}" not in member_dirs:
            fail(f"{top}/{entry} is not a workspace member")

def internal_deps(p):
    # (dependency package name, kind) for path dependencies on workspace crates.
    for d in p["dependencies"]:
        if d["name"] in pkgs and d["name"] != p["name"]:
            yield d["name"], (d["kind"] or "normal")

# ---- crates/ never builds on testing/ -------------------------------------
for name, p in pkgs.items():
    if not rel(p).startswith("crates/"):
        continue
    for dep, kind in internal_deps(p):
        if kind != "dev" and rel(pkgs[dep]).startswith("testing/"):
            fail(f"{name}: {kind} dependency on {dep}; crates/ depends on testing/")

# ---- layering --------------------------------------------------------------
for name, p in pkgs.items():
    if not rel(p).startswith("crates/"):
        continue
    if name not in layer:
        fail(f"{name} is not in LAYERS; place it (scripts/check-layout.sh)")
        continue
    for dep, kind in internal_deps(p):
        if kind == "dev" or dep not in layer:
            continue
        if layer[dep] >= layer[name]:
            fail(f"{name} -> {dep} ({kind}) points up or sideways (layer {layer[name]} -> {layer[dep]}); dependencies point down")

def deps_of(name, kinds=("normal", "build", "dev")):
    return {d for d, k in internal_deps(pkgs[name]) if k in kinds}

def only(name, allowed, kinds=("normal", "build")):
    extra = deps_of(name, kinds) - set(allowed)
    if extra:
        what = f"{sorted(allowed)} only" if allowed else "no workspace crate"
        fail(f"{name} may depend on {what}; it depends on {sorted(extra)}")

for n in ("arc-lend", "tutti-ump"):
    if n in pkgs:
        only(n, [], ("normal", "build", "dev"))
if "tutti-types" in pkgs:
    only("tutti-types", [])
if "tutti-rt" in pkgs:
    only("tutti-rt", ["arc-lend"])
if "tutti-midi-protocol" in pkgs:
    only("tutti-midi-protocol", ["tutti-ump"])
for n in ("tutti-dsp", "tutti-mod"):
    if n in pkgs:
        only(n, ["tutti-types"])
for n in ("tutti-graph", "tutti-midi-types"):
    if n in pkgs:
        only(n, ["tutti-types", "tutti-rt", "tutti-ump"])
if "tutti-nodes" in pkgs and "tutti-spatial" in deps_of("tutti-nodes"):
    fail("tutti-nodes must not depend on tutti-spatial (spatial depends on nodes)")
if "tutti-spatial" in pkgs and "tutti-nodes" not in deps_of("tutti-spatial"):
    fail("tutti-spatial must depend on tutti-nodes")
for name in pkgs:
    users = {d for d, _ in internal_deps(pkgs[name])}
    if "tutti-session" in users and name not in ("tutti", "bevy-tutti"):
        fail(f"{name} depends on tutti-session; only tutti and bevy-tutti do")
    for top in ("tutti", "bevy-tutti"):
        if top in users:
            fail(f"{name} depends on {top}, the top of the tree")

if errors:
    print("check-layout: FAIL")
    for e in errors:
        print("  " + e)
    sys.exit(1)
print(f"check-layout: ok ({len(pkgs)} members, {len(layer)} ranked)")
PY

python3 -I - <<'PY'
import os, re, sys

# A path component `..` in a string literal, `.ancestors()` or `.parent()`
# near the manifest directory: all depend on how deep a crate sits.
# Exempt: the macros that resolve against the source file (`include_str!`,
# `include!`, `include_bytes!`, `#[path = ".."]`), which are crate-relative by
# design, and build scripts (a build-script path). EXEMPT_FILES lists any other
# legitimate use, with the reason.
EXEMPT_FILES = {
    # The walk itself: it climbs from the manifest dir to find the repo root.
    "testing/tutti-fixtures/src/lib.rs": "implements repo_root and profile_dir",
}

root = os.getcwd()
literal = r'"(?:[^"\\]|\\.)*"'
source_relative = re.compile(
    r'(?:include_str|include_bytes|include)!\(\s*' + literal + r'\s*\)|#\[path\s*=\s*' + literal + r'\s*\]'
)
has_dotdot = lambda lit: ".." in re.split(r'[/\\]', lit[1:-1])
path_call = re.compile(r'(?:Path::new|PathBuf::from|\.join|\.push|\.starts_with|concat!)\(\s*(' + literal + ')')
bad = []
def report(path, text, pos, why):
    bad.append((path, text.count("\n", 0, pos) + 1, why))

for top in ("crates", "testing"):
    for dirpath, _, files in os.walk(os.path.join(root, top)):
        for name in files:
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            if not name.endswith(".rs") or name == "build.rs" or rel in EXEMPT_FILES:
                continue
            raw = open(path, encoding="utf-8").read()
            # Blank the crate-relative macros, keeping offsets and newlines.
            text = source_relative.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), raw)
            mentions_manifest = "CARGO_MANIFEST_DIR" in text
            for m in re.finditer(literal, text):
                lit = m.group(0)
                if has_dotdot(lit) and (mentions_manifest or path_call.search(text[max(0, m.start() - 40): m.end()])):
                    report(path, text, m.start(), f"{lit} climbs with `..`")
                elif re.search(r'(?:\.\./)+[^"]*assets', lit):
                    report(path, text, m.start(), f"{lit} climbs to assets")
            if mentions_manifest:
                for m in re.finditer(r'\.(?:ancestors|parent)\(\)', text):
                    report(path, text, m.start(), f"`{m.group(0)}` in a file that reads CARGO_MANIFEST_DIR")
if bad:
    print("check-layout: FAIL")
    for path, line, why in bad:
        print(f"  {os.path.relpath(path, root)}:{line}: depth-dependent path ({why}); use tutti_fixtures::asset or repo_root")
    sys.exit(1)
print("check-layout: no depth-dependent test paths")
PY
