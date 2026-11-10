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
# - tutti-vst2-ffi: each
#   `extern "C" fn NAME` (generics allowed) defined outside test code in
#   `src/host.rs` runs its body under `catch_unwind(..)`. The audioMaster
#   callback (`callback_wrapper`) is the only function a VST2 plugin calls in
#   the host; the plugin side (`dispatch.rs`, `lib.rs`) is
#   `testing/tutti-vst2-author`'s, and `tutti-vst2-host` defines none.
#
# The functions in ALLOW only return a constant or have an empty body, so
# there is nothing to guard. A new entry needs that argument.
#
# Mutation (run): replace the `guarded(host, (), |cb| cb.request_restart())`
# body of `host_request_restart` in clap-ffi with `()`, or delete the wrapper
# of `getEventCount` in vst3-ffi, or the `catch_unwind` of `callback_wrapper`
# in tutti-vst2-ffi -> this script exits 1 naming the function.
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


# The host callbacks CLAP marks `[main-thread]` (`ext/*.h`): each must be a
# `main_only` trampoline, and every other trampoline a `guarded` one, so a
# plugin that calls one off the main thread is refused and a `[thread-safe]`
# one never is.
MAIN_THREAD = {
    "host_ambisonic_changed",
    "host_audio_ports_config_rescan",
    "host_audio_ports_rescan",
    "host_latency_changed",
    "host_note_name_changed",
    "host_note_ports_rescan",
    "host_params_rescan",
    "host_posix_fd_modify",
    "host_posix_fd_register",
    "host_posix_fd_unregister",
    "host_preset_load_loaded",
    "host_remote_controls_changed",
    "host_remote_controls_suggest_page",
    "host_resource_release_directory",
    "host_resource_request_directory",
    "host_state_mark_dirty",
    "host_surround_changed",
    "host_timer_register",
    "host_timer_unregister",
    "host_transport_request_continue",
    "host_transport_request_enable_loop",
    "host_transport_request_jump",
    "host_transport_request_loop_region",
    "host_transport_request_pause",
    "host_transport_request_record",
    "host_transport_request_start",
    "host_transport_request_stop",
    "host_transport_request_toggle_loop",
    "host_transport_request_toggle_play",
    "host_transport_request_toggle_record",
    "host_triggers_rescan",
    "host_tuning_get_count",
    "host_undo_begin_change",
    "host_undo_cancel_change",
    "host_undo_change_made",
    "host_undo_request_redo",
    "host_undo_request_undo",
    "host_undo_set_wants_context",
    "host_voice_info_changed",
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
# Mutation (run): change one `guarded` to `unchecked`, or `host_params_rescan`
# from `main_only` to `guarded`, or a `[thread-safe]` one to `main_only` -> exits 1
# naming the callback.
for path in matches("crates/tutti-clap-ffi/src/**/*.rs"):
    s = open(path).read()
    t = re.search(r"^\s*#\[cfg\(test\)\]\s*\n\s*(pub )?mod \w+", s, re.M)
    s = s[: t.start()] if t else s
    for m in re.finditer(r"^trampoline!\(\s*(\w+)\(", s, re.M):
        seen += 1
        end = match(s, m.end() - 1, "(", ")")
        kind = re.match(r"\s*,?\s*(?:->[^,]*)?,?\s*(\w+)\s*,", s[end + 1 :])
        want = "main_only" if m.group(1) in MAIN_THREAD else "guarded"
        if kind is None or kind.group(1) != want:
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
for path in matches("crates/tutti-vst2-ffi/src/host.rs"):
    s = open(path).read()
    t = re.search(r"^\s*#\[cfg\(test\)\]\s*\n\s*(pub )?mod \w+", s, re.M)
    s = s[: t.start()] if t else s
    for m in re.finditer(r'(?:unsafe )?extern "C" fn (\w+)(<[^>(]*>)?\(', s):
        seen += 1
        vst2_seen += 1
        if m.group(1) not in ALLOW and re.search(r"\bcatch_unwind\(", body_of(s, m.end())) is None:
            bad.append((path, m.group(1)))
if vst2_seen == 0:
    sys.exit("check-guards: no VST2 host callback found in tutti-vst2-ffi/src/host.rs; the scan is broken")

if seen < 100:
    sys.exit(f"check-guards: only {seen} callbacks found; the scan is broken")
for path, name in bad:
    print(f"unguarded host callback {name} in {path}", file=sys.stderr)
if bad:
    sys.exit(1)
print(f"check-guards: {seen} callbacks, all guarded or allow-listed")
PY
