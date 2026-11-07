#!/usr/bin/env bash
#
# A tuple struct with a private field must not derive `Deserialize` plainly:
# the derive builds the value without `new`, so a document can carry a raw the
# type refuses (CLAUDE.md, "Range-checked values"). The derive is fine with
# `serde(try_from = ..)`, `serde(from = ..)` or `serde(transparent)`, or when
# the struct is on EXCEPTIONS.
#
# EXCEPTIONS is the debt, one entry per type with what removes it. An entry
# whose type no longer matches is an error, so the list cannot rot.
#
# Limits: a textual scan of `struct Name(..)` followed by its attributes. It
# sees tuple structs only (not named fields) and a derive spelled in
# `#[derive]` or `#[cfg_attr(.., derive(..))]`; a hand-written `impl
# Deserialize` is not checked. Test files (`tests/`) are skipped.
#
# Mutation record (planted at the end of `tutti-types/src/lib.rs`, seen to exit
# 1, removed):
# - `#[derive(serde::Deserialize)] struct P(u8);`: flagged;
# - `#[cfg_attr(feature = "serde", derive(serde::Deserialize))] pub struct P(pub(crate) u8);`:
#   flagged (multi-line attribute, `pub(crate)` is private to the type's API);
# - the same with `serde(try_from = "u8")`: passes;
# - an EXCEPTIONS entry for a type that does not exist: "stale exception".
#
# Needs python3. No build.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 -I - <<'PY'
import os, re, sys

# (crate-relative path, type): what removes the entry.
EXCEPTIONS = {
    ("crates/tutti-midi-types/src/mpe/sensitivity.rs", "PitchBendSensitivity"):
        "R2a: serde through try_from",
    ("crates/tutti-plugin-types/src/parameters.rs", "Normalized"):
        "R2b: serde through try_from",
    ("crates/tutti-types/src/channels.rs", "ChannelLayout"):
        "permanent: every u16 is a valid layout, so the derive cannot admit a bad raw",
}

# `struct Name(` whose first field is not `pub` (`pub(..)` is not public).
tuple = re.compile(r'^[ \t]*(?:#\[.*?\][ \t]*)*(?:pub(?:\([^)]*\))?[ \t]+)?struct[ \t]+(\w+)(?:<[^>]*>)?[ \t]*\([ \t\n]*(pub(?![\w(])|\))?', re.M)
plain_ok = re.compile(r'try_from|\bfrom[ \t]*=|transparent')

def attrs_before(text, pos):
    # The attributes directly above `pos`, walking back over `#[..]` (matched
    # brackets, any number of lines) and `//` lines.
    out, end = [], pos
    while True:
        head = text[:end].rstrip()
        last = head.rsplit("\n", 1)[-1].strip()
        if last.startswith("//"):
            end = len(head) - len(last)
            continue
        if not head.endswith("]"):
            return "\n".join(out)
        depth, i = 0, len(head) - 1
        while i >= 0:
            depth += {"]": 1, "[": -1}.get(head[i], 0)
            if depth == 0:
                break
            i -= 1
        if i < 1 or head[i - 1] != "#":
            return "\n".join(out)
        out.append(head[i - 1:])
        end = i - 1

bad, seen = [], set()
for top in ("crates", "testing"):
    for dirpath, dirs, files in os.walk(top):
        dirs[:] = [d for d in dirs if d not in ("tests", "target", "benches")]
        for name in files:
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            text = open(path, encoding="utf-8").read()
            for m in tuple.finditer(text):
                if m.group(2):  # `pub` field or unit-like `()`
                    continue
                a = attrs_before(text, m.start()) + m.group(0)
                if "Deserialize" not in a or plain_ok.search(a):
                    continue
                key = (path, m.group(1))
                line = text.count("\n", 0, m.start()) + 1
                if key in EXCEPTIONS:
                    seen.add(key)
                else:
                    bad.append(f"{path}:{line}: {m.group(1)} derives Deserialize on a private field; use serde(try_from = ..)")
for key, why in EXCEPTIONS.items():
    if key not in seen:
        bad.append(f"stale exception {key[1]} in {key[0]} ({why})")

if bad:
    print("check-range-types: FAIL")
    for b in bad:
        print("  " + b)
    sys.exit(1)
print(f"check-range-types: ok ({len(seen)} excepted)")
PY
