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
#   `testing/` (a dev-dependency is fine), except the edges in
#   KNOWN_TESTING_BUILD_EDGES.
#
# Layering (CLAUDE.md, "Workspace"): a normal or build dependency between
# workspace crates points strictly down the LAYERS table. A crate missing from
# the table is an error: placing it is the point. On top of that:
# - `arc-lend` depends on no workspace crate, of any kind;
# - `tutti-types` depends on no tutti crate but `arc-lend`;
# - `tutti-dsp`, `tutti-graph`, `tutti-mod` and `tutti-midi-types` depend on
#   `tutti-types` alone;
# - `tutti-spatial` depends on `tutti-nodes`, and `tutti-nodes` never on
#   `tutti-spatial`, of any kind;
# - nothing but `tutti` and `bevy-tutti` depends on `tutti-session`, and
#   nothing depends on `tutti` or `bevy-tutti`.
#
# Mutation record (each planted on the tree that added this script, seen to
# exit 1, then removed):
# - `tutti-nodes` as a `[dependencies]` entry of `tutti-types`: "tutti-types ->
#   tutti-nodes (normal) points up" and "tutti-types may depend on ['arc-lend']
#   only";
# - `tutti-spatial` as a `[dev-dependencies]` entry of `tutti-nodes`:
#   "tutti-nodes must not depend on tutti-spatial";
# - `tutti-fixture-resolve` as a `[dependencies]` entry of `tutti-dsp`:
#   "normal dependency on tutti-fixture-resolve; crates/ depends on testing/";
# - `tutti-mod` moved to `crates/nest/tutti-mod`: "is a nested directory under
#   crates/" (a nested directory under crates/ is refused);
# - `tutti-mod` moved to `crates/modx`: "must be crates/tutti-mod";
# - `mkdir crates/stray`: "crates/stray is not a workspace member";
# - `tutti-graph` deleted from LAYERS: "tutti-graph is not in LAYERS".
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
    ["arc-lend", "vst-tutti", "tutti-alsa-ump-sys", "tutti-rustysynth"],
    ["tutti-types"],
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

# crates/ -> testing/ build edges that exist today: each of these crates'
# build.rs resolves the test plugin cdylib a dev-dependency builds, through
# tutti-fixture-resolve. Real debt (a shipped crate builds a testing crate);
# the edge is listed so the rule bites on every new one, and an entry whose
# edge is gone fails, so the list cannot outlive the debt.
KNOWN_TESTING_BUILD_EDGES = {
    ("tutti-clap-ffi", "tutti-fixture-resolve"),
    ("tutti-clap-host", "tutti-fixture-resolve"),
    ("tutti-plugin", "tutti-fixture-resolve"),
    ("tutti-plugin-server", "tutti-fixture-resolve"),
    ("tutti-vst2-host", "tutti-fixture-resolve"),
}

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
seen_known = set()
for name, p in pkgs.items():
    if not rel(p).startswith("crates/"):
        continue
    for dep, kind in internal_deps(p):
        if kind == "dev" or not rel(pkgs[dep]).startswith("testing/"):
            continue
        if (name, dep) in KNOWN_TESTING_BUILD_EDGES:
            seen_known.add((name, dep))
        else:
            fail(f"{name}: {kind} dependency on {dep}; crates/ depends on testing/")
for edge in sorted(KNOWN_TESTING_BUILD_EDGES - seen_known):
    fail(f"KNOWN_TESTING_BUILD_EDGES lists {edge[0]} -> {edge[1]} but the edge is gone; remove it")

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
        what = sorted(allowed) if allowed else "no workspace crate"
        fail(f"{name} may depend on {what} only; it depends on {sorted(extra)}")

if "arc-lend" in pkgs:
    only("arc-lend", [], ("normal", "build", "dev"))
if "tutti-types" in pkgs:
    only("tutti-types", ["arc-lend"])
for n in ("tutti-dsp", "tutti-graph", "tutti-mod", "tutti-midi-types"):
    if n in pkgs:
        only(n, ["tutti-types"])
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
