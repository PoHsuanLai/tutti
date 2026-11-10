#!/usr/bin/env bash
#
# Every host callback a plugin can call into must run its body in the panic
# guard (`guard(..)`, its `guarded(..)` wrapper around a `HostCallbacks`
# call, `main_only(..)`, which is `guarded` behind a thread check, or, for `request_exec`, `or_abort(..)`), because a panic
# out of an `extern "C"` / COM method aborts the process:
#
# - tutti-clap-ffi: each `extern "C" fn NAME(` defined outside test code (text
#   from the first `#[cfg(test)] mod` of a file on is skipped). The host
#   callbacks, the stream, event-list and context-menu callbacks all live
#   there; `tutti-clap-host` has none.
# - tutti-au-ffi: each `extern "C" fn NAME(` defined outside test code runs
#   its body under `guard(..)` (text match, as for tutti-clap-ffi).
# - tutti-vst3-ffi: each `unsafe fn` in an `impl ...Trait for ...` block
#   under `src/com`.
# - vst-tutti (the VST2 `-ffi` crate that is not split yet): each
#   `extern "C" fn NAME` (generics allowed) defined outside test code in
#   `src/host.rs` runs its body under `catch_unwind(..)`. The audioMaster
#   callback (`callback_wrapper`) is the only function a VST2 plugin calls in
#   the host; the other `extern "C" fn`s in the crate are the plugin side
#   (`interfaces.rs`, `lib.rs`), which the host calls, and
#   `tutti-vst2-host` defines none.
#
# The functions in ALLOW only return a constant or have an empty body, so
# there is nothing to guard. A new entry needs that argument.
#
# Mutation (run): replace the `guarded(host, (), |cb| cb.request_restart())`
# body of `host_request_restart` in clap-ffi with `()`, or delete the wrapper
# of `getEventCount` in vst3-ffi, or the `catch_unwind` of `callback_wrapper`
# in vst-tutti -> this script exits 1 naming the function.
#
# No build needed (python3 only), so CI runs it in the static `paths` job.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import glob, re, sys

ALLOW = {
    # clap-ffi: constant answers or empty bodies
    "host_params_clear", "host_gui_request_show", "host_gui_request_hide",
    "host_note_ports_supported_dialects", "host_preset_load_on_error",
    "host_context_menu_populate", "host_context_menu_perform",
    "host_context_menu_can_popup", "host_context_menu_popup",
    "host_triggers_clear", "host_tuning_get_relative",
    "host_tuning_should_play",
    # vst3-ffi
    "createContextMenu",
}


def skip(s, i):
    """Index after the comment/string/char literal at i, or i if none."""
    if s.startswith("//", i):
        j = s.find("\n", i)
        return j if j >= 0 else len(s)
    if s.startswith("/*", i):
        return s.find("*/", i) + 2
    if s[i] == '"':
        j = i + 1
        while s[j] != '"':
            j += 2 if s[j] == "\\" else 1
        return j + 1
    if s[i] == "'":
        m = re.match(r"'(\\.[^']*|[^\\'])'", s[i:])
        if m:
            return i + m.end()
    return i


def match(s, i, o, c):
    d = 0
    while True:
        j = skip(s, i)
        if j != i:
            i = j
            continue
        if s[i] == o:
            d += 1
        elif s[i] == c:
            d -= 1
            if d == 0:
                return i
        i += 1


def body_of(s, name_end):
    p = match(s, name_end - 1, "(", ")")
    b = s.index("{", p)
    return s[b : match(s, b, "{", "}") + 1]


def guarded(body):
    return re.search(r"\b(guard|guarded|main_only|or_abort|with_state|with_state_mut)(::<[^(]*>)?\(", body) is not None


bad, seen = [], 0


# Mutation (run): pointing either glob at a wrong crate path (`tutti-clap-fff`,
# `src/comm`) -> exits 1, "matches no file".
def matches(pattern):
    """The files `pattern` names; a pattern that names none is a moved path,
    and a scan of nothing would pass."""
    paths = glob.glob(pattern, recursive=True)
    if not paths:
        sys.exit(f"check-guards: {pattern} matches no file; was the crate moved?")
    return paths


for path in matches("crates/tutti-clap-ffi/src/**/*.rs"):
    s = open(path).read()
    t = re.search(r"^\s*#\[cfg\(test\)\]\s*\n\s*(pub )?mod \w+", s, re.M)
    s = s[: t.start()] if t else s
    for m in re.finditer(r'(?:unsafe )?extern "C" fn (\w+)\(', s):
        seen += 1
        if m.group(1) not in ALLOW and not guarded(body_of(s, m.end())):
            bad.append((path, m.group(1)))

# A `trampoline!` (clap-ffi's host.rs) expands to a callback under `guarded` or
# `main_only`; the kind is the second argument, so that is what is checked.
# Mutation (run): change one `guarded` to `unchecked` in an invocation -> exits 1.
for path in matches("crates/tutti-clap-ffi/src/**/*.rs"):
    s = open(path).read()
    t = re.search(r"^\s*#\[cfg\(test\)\]\s*\n\s*(pub )?mod \w+", s, re.M)
    s = s[: t.start()] if t else s
    for m in re.finditer(r"^trampoline!\(\s*(\w+)\(", s, re.M):
        seen += 1
        end = match(s, m.end() - 1, "(", ")")
        kind = re.match(r"\s*,?\s*(?:->[^,]*)?,?\s*(\w+)\s*,", s[end + 1 : end + 80] if False else s[end + 1 :])
        if kind is None or kind.group(1) not in ("guarded", "main_only"):
            bad.append((path, m.group(1)))

for path in matches("crates/tutti-au-ffi/src/**/*.rs"):
    s = open(path).read()
    t = re.search(r"^\s*#\[cfg\(test\)\]\s*\n\s*(pub )?mod \w+", s, re.M)
    s = s[: t.start()] if t else s
    for m in re.finditer(r'(?:unsafe )?extern "C" fn (\w+)\(', s):
        seen += 1
        if m.group(1) not in ALLOW and not guarded(body_of(s, m.end())):
            bad.append((path, m.group(1)))

for path in matches("crates/tutti-vst3-ffi/src/com/*.rs"):
    s = open(path).read()
    t = re.search(r"^\s*#\[cfg\(test\)\]\s*\n\s*(pub )?mod \w+", s, re.M)
    s = s[: t.start()] if t else s
    for im in re.finditer(r"^impl [\w:]*Trait for [^{]*\{", s, re.M):
        ie = match(s, im.end() - 1, "{", "}")
        for m in re.finditer(r"unsafe fn (\w+)(<[^>]*>)?\(", s[im.end() : ie]):
            seen += 1
            end = im.end() + m.end()
            if m.group(1) not in ALLOW and not guarded(body_of(s, end)):
                bad.append((path, m.group(1)))

# VST2: the one audioMaster callback, which must be under `catch_unwind`.
vst2_seen = 0
for path in matches("crates/vst-tutti/src/host.rs"):
    s = open(path).read()
    t = re.search(r"^\s*#\[cfg\(test\)\]\s*\n\s*(pub )?mod \w+", s, re.M)
    s = s[: t.start()] if t else s
    for m in re.finditer(r'(?:unsafe )?extern "C" fn (\w+)(<[^>(]*>)?\(', s):
        seen += 1
        vst2_seen += 1
        if m.group(1) not in ALLOW and re.search(r"\bcatch_unwind\(", body_of(s, m.end())) is None:
            bad.append((path, m.group(1)))
if vst2_seen == 0:
    sys.exit("check-guards: no VST2 host callback found in vst-tutti/src/host.rs; the scan is broken")

if seen < 100:
    sys.exit(f"check-guards: only {seen} callbacks found; the scan is broken")
for path, name in bad:
    print(f"unguarded host callback {name} in {path}", file=sys.stderr)
if bad:
    sys.exit(1)
print(f"check-guards: {seen} callbacks, all guarded or allow-listed")
PY
