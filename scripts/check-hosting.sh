#!/usr/bin/env bash
#
# Every hosting marker a format implements is listed in the plugin contract
# (design 021 §3.3). `impl InProcess for X` or `impl OutOfProcess for X` in a
# crate's `src/` needs a contract suite whose format's loaded type is `X` and
# whose `contract!(.., hostings = [..])` names that marker. The other direction
# is the compiler's: `contract!` asserts each marker it lists, so a listed
# marker without its impl does not build.
#
# The markers are safe and unsealed, so an impl is a promise no type checks;
# the contract's cases are what hold a format to it. An impl the contract does
# not list would be hostable without being tested in that hosting.
#
# Limits: a textual scan. It sees `impl [<..>] [path::]InProcess for [path::]Name`
# on one line in `crates/*/src`, maps `Name` to a contract format through
# `type L = ..::Name;` in `testing/tutti-plugin-contract/src/formats.rs`, and
# reads the `contract!` calls in `testing/tutti-plugin-contract/tests/*.rs`
# (which may span lines). Test files are skipped.
#
# Mutation record (seen to exit 1, restored):
# - `hostings = [InProcess]` for `Clap` in `tests/clap.rs`: "ClapLoaded
#   implements OutOfProcess, but no contract lists it";
# - `type L = tutti_clap_host::ClapLoaded;` renamed to `ClapLoadedX` in
#   `formats.rs` (scan only): "ClapLoaded implements InProcess, but no contract
#   format loads it".
#
# Needs python3. No build.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'EOF'
import glob, re, sys

MARKERS = ("InProcess", "OutOfProcess")

impl_re = re.compile(
    r"^\s*impl(?:\s*<[^>]*>)?\s+(?:[\w:]+::)?(InProcess|OutOfProcess)\s+for\s+(?:[\w:]+::)?(\w+)"
)
impls = []  # (marker, type, file:line)
for path in sorted(glob.glob("crates/*/src/**/*.rs", recursive=True)):
    with open(path) as f:
        for n, line in enumerate(f, 1):
            m = impl_re.match(line)
            if m:
                impls.append((m.group(1), m.group(2), f"{path}:{n}"))

formats = {}  # loaded type -> [format names]
src = open("testing/tutti-plugin-contract/src/formats.rs").read()
for m in re.finditer(r"impl\s+Format\s+for\s+(\w+)\s*\{(.*?)\n\}", src, re.S):
    t = re.search(r"type\s+L\s*=\s*(?:[\w:]+::)?(\w+)\s*;", m.group(2))
    if t:
        formats.setdefault(t.group(1), []).append(m.group(1))

listed = {}  # format name -> set of markers
for path in sorted(glob.glob("testing/tutti-plugin-contract/tests/*.rs")):
    text = open(path).read()
    for m in re.finditer(r"contract!\s*\((.*?)\)\s*;", text, re.S):
        args = m.group(1)
        fmt = re.match(r"\s*(?:[\w:]+::)?(\w+)\s*,", args)
        hostings = re.search(r"hostings\s*=\s*\[([^\]]*)\]", args)
        if fmt:
            names = set(re.findall(r"\w+", hostings.group(1))) if hostings else set()
            listed.setdefault(fmt.group(1), set()).update(names)

if not impls:
    sys.exit("check-hosting: found no InProcess/OutOfProcess impl; the scan is broken")

errors = []
for marker, ty, where in impls:
    fmts = formats.get(ty)
    if not fmts:
        errors.append(f"{where}: {ty} implements {marker}, but no contract format loads it")
        continue
    if not any(marker in listed.get(f, set()) for f in fmts):
        errors.append(
            f"{where}: {ty} implements {marker}, but no contract lists it "
            f"(contract!({'|'.join(fmts)}, .., hostings = [.., {marker}]))"
        )

for e in errors:
    print(e, file=sys.stderr)
if errors:
    sys.exit(1)
print(f"check-hosting: {len(impls)} hosting impls, each listed in the contract")
EOF
