//! `check-guards`: every host callback a plugin can call into runs its body
//! in the panic guard, because a panic out of an `extern "C"` function or a
//! COM method aborts the process.
//!
//! - `tutti-clap-ffi`: each `extern "C" fn NAME(` outside test code (the text
//!   from the first `#[cfg(test)] mod` of a file on is skipped) runs under
//!   `guard(..)`, `guarded(..)`, `main_only(..)`, `or_abort(..)`,
//!   `with_state(..)` or `with_state_mut(..)`. Each `trampoline!(NAME(..), KIND, ..)`
//!   names `main_only` for a callback CLAP marks `[main-thread]`
//!   ([`MAIN_THREAD`]) and `guarded` for every other, so a plugin that calls
//!   one off the main thread is refused and a `[thread-safe]` one never is.
//! - `tutti-au-ffi`: each `extern "C" fn NAME(` outside test code, the same
//!   guards.
//! - `tutti-vst3-ffi`: each `unsafe fn` in an `impl ...Trait for ...` block
//!   under `src/com`.
//! - `tutti-vst2-ffi`: each `extern "C" fn` in `src/host.rs` runs under
//!   `catch_unwind(..)`: the audioMaster callback is the only function a VST2
//!   plugin calls in the host.
//!
//! [`ALLOW`] lists the functions that only return a constant or have an empty
//! body, so there is nothing to guard; a new entry needs that argument.
//! A glob that matches no file is a moved path, and a scan of nothing would
//! pass, so it fails.
//!
//! Limits: a textual scan over the source with comments and literals blanked
//! (a hit inside a comment is not a callback, and a guard named only in a
//! comment is not a guard).

use super::lex::{blank, find_word_from, matching};
use super::{Report, Tree};

const SKIPPED_DIRS: &[&str] = &["target", ".git"];

fn skipped(dir: &str) -> bool {
    SKIPPED_DIRS.contains(&dir)
}

/// Callbacks with nothing to guard: constant answers or empty bodies.
const ALLOW: &[&str] = &[
    // clap-ffi
    "host_params_clear",
    "host_gui_request_show",
    "host_gui_request_hide",
    "host_note_ports_supported_dialects",
    "host_preset_load_on_error",
    "host_context_menu_populate",
    "host_context_menu_perform",
    "host_context_menu_can_popup",
    "host_context_menu_popup",
    "host_triggers_clear",
    "host_tuning_get_relative",
    "host_tuning_should_play",
    // vst3-ffi
    "createContextMenu",
];

/// The host callbacks CLAP marks `[main-thread]` (`ext/*.h`).
const MAIN_THREAD: &[&str] = &[
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
];

const GUARDS: &[&str] = &[
    "guard",
    "guarded",
    "main_only",
    "or_abort",
    "with_state",
    "with_state_mut",
];

/// The fewest callbacks the real tree must show, or the scan is broken.
pub(super) const MIN_SEEN: usize = 100;

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The text before the first `#[cfg(test)]` followed by a `mod` item (a
/// `#[cfg(test)] use` or fn above production code does not end the scan).
fn production(s: &str) -> &str {
    let mut ls = 0;
    for line in s.split_inclusive('\n') {
        let rest = s[ls..].trim_start();
        if let Some(after) = rest.strip_prefix("#[cfg(test)]") {
            let trimmed = after.trim_start();
            let gap = &after[..after.len() - trimmed.len()];
            let item = trimmed.strip_prefix("pub ").unwrap_or(trimmed);
            if gap.contains('\n')
                && item
                    .strip_prefix("mod ")
                    .is_some_and(|n| n.bytes().next().is_some_and(is_word))
            {
                return &s[..ls];
            }
        }
        ls += line.len();
    }
    s
}

/// The identifier at the start of `s`.
fn ident(s: &str) -> &str {
    let n = s.bytes().take_while(|&b| is_word(b)).count();
    &s[..n]
}

/// The body `{ .. }` of the function whose parameter list opens at `paren`
/// (in blanked text).
fn body_of(code: &str, paren: usize) -> Option<&str> {
    let close = matching(code, paren, b'(', b')')?;
    let open = close + code[close..].find('{')?;
    let end = matching(code, open, b'{', b'}')?;
    Some(&code[open..=end])
}

/// `\b(guard|guarded|..)(::<[^(]*>)?\(` in a blanked body.
fn guarded(body: &str) -> bool {
    GUARDS.iter().any(|g| {
        let mut from = 0;
        while let Some(at) = find_word_from(body, g, from) {
            let rest = &body[at + g.len()..];
            let call = rest.starts_with('(')
                || rest
                    .strip_prefix("::<")
                    .is_some_and(|t| t.find('(').is_some_and(|p| p > 0 && t[..p].ends_with('>')));
            if call {
                return true;
            }
            from = at + 1;
        }
        false
    })
}

fn catches_unwind(body: &str) -> bool {
    let mut from = 0;
    while let Some(at) = find_word_from(body, "catch_unwind", from) {
        if body[at + "catch_unwind".len()..].starts_with('(') {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Each `extern "C" fn NAME[<..>](` of `raw`, as `(name, offset of its '(')`,
/// not in a comment. `generics` allows `NAME<..>(`.
fn extern_fns(raw: &str, code: &str, generics: bool) -> Vec<(String, usize)> {
    let needle = "extern \"C\" fn ";
    let mut out = Vec::new();
    for (at, _) in raw.match_indices(needle) {
        if code.as_bytes()[at] == b' ' {
            continue;
        }
        let rest = &raw[at + needle.len()..];
        let name = ident(rest);
        if name.is_empty() {
            continue;
        }
        let mut p = at + needle.len() + name.len();
        if generics && raw[p..].starts_with('<') {
            let Some(gt) = raw[p..].find(['>', '(']) else {
                continue;
            };
            if !raw[p + gt..].starts_with('>') {
                continue;
            }
            p += gt + 1;
        }
        if raw[p..].starts_with('(') {
            out.push((name.to_owned(), p));
        }
    }
    out
}

struct Scan<'a> {
    tree: &'a dyn Tree,
    bad: Vec<String>,
    seen: usize,
}

impl Scan<'_> {
    /// The files under `dir` ending `.rs`, or all of them recursively;
    /// `direct` keeps only `dir`'s own. None named is a moved path.
    fn files(&mut self, dir: &str, direct: bool) -> Vec<String> {
        let files: Vec<String> = self
            .tree
            .files(dir, &skipped)
            .into_iter()
            .filter(|p| p.ends_with(".rs") && (!direct || !p[dir.len() + 1..].contains('/')))
            .collect();
        if files.is_empty() {
            self.bad
                .push(format!("{dir} matches no file; was the crate moved?"));
        }
        files
    }

    fn text(&self, path: &str) -> String {
        self.tree.read(path).unwrap_or_default()
    }

    fn flag(&mut self, name: &str, path: &str) {
        self.bad
            .push(format!("unguarded host callback {name} in {path}"));
    }

    /// clap-ffi and au-ffi: every `extern "C" fn` under a guard.
    fn guarded_externs(&mut self, dir: &str) {
        for path in self.files(dir, false) {
            let text = self.text(&path);
            let raw = production(&text);
            let code = blank(raw);
            for (name, paren) in extern_fns(raw, &code, false) {
                self.seen += 1;
                if !ALLOW.contains(&name.as_str()) && !body_of(&code, paren).is_some_and(guarded) {
                    self.flag(&name, &path);
                }
            }
        }
    }

    fn trampolines(&mut self, dir: &str) {
        for path in self.files(dir, false) {
            let text = self.text(&path);
            let raw = production(&text);
            let code = blank(raw);
            let mut ls = 0;
            for line in raw.split_inclusive('\n') {
                let at = ls;
                ls += line.len();
                if !line.starts_with("trampoline!(") || code.as_bytes()[at] == b' ' {
                    continue;
                }
                // `\s*` runs across lines: the name may sit below the `(`.
                let rest = &raw[at + "trampoline!(".len()..];
                let after_ws = rest.trim_start();
                let name = ident(after_ws);
                let open = at + "trampoline!(".len() + (rest.len() - after_ws.len()) + name.len();
                if name.is_empty() || !raw[open..].starts_with('(') {
                    continue;
                }
                self.seen += 1;
                let want = if MAIN_THREAD.contains(&name) {
                    "main_only"
                } else {
                    "guarded"
                };
                let kind = matching(&code, open, b'(', b')')
                    .and_then(|end| trampoline_kind(&code[end + 1..]));
                if kind != Some(want) {
                    self.flag(name, &path);
                }
            }
        }
    }

    fn vst3(&mut self, dir: &str) {
        for path in self.files(dir, true) {
            let text = self.text(&path);
            let raw = production(&text);
            let code = blank(raw);
            let mut ls = 0;
            for line in raw.split_inclusive('\n') {
                let at = ls;
                ls += line.len();
                let Some(rest) = line.strip_prefix("impl ") else {
                    continue;
                };
                // `[\w:]*Trait for `: the maximal path run ends in `Trait`.
                let run = rest
                    .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                    .unwrap_or(rest.len());
                if !(rest[..run].ends_with("Trait") && rest[run..].starts_with(" for ")) {
                    continue;
                }
                let Some(open) = code[at..].find('{').map(|p| at + p) else {
                    continue;
                };
                let Some(end) = matching(&code, open, b'{', b'}') else {
                    continue;
                };
                let block = &code[open..end];
                let needle = "unsafe fn ";
                for (p, _) in block.match_indices(needle) {
                    let name = ident(&block[p + needle.len()..]);
                    let mut q = open + p + needle.len() + name.len();
                    if name.is_empty() {
                        continue;
                    }
                    if code[q..].starts_with('<') {
                        let Some(gt) = code[q..].find('>') else {
                            continue;
                        };
                        q += gt + 1;
                    }
                    if !code[q..].starts_with('(') {
                        continue;
                    }
                    self.seen += 1;
                    if !ALLOW.contains(&name) && !body_of(&code, q).is_some_and(guarded) {
                        self.flag(name, &path);
                    }
                }
            }
        }
    }

    fn vst2(&mut self, path: &str) -> usize {
        let mut n = 0;
        let text = self.tree.read(path);
        if text.is_none() {
            self.bad
                .push(format!("{path} matches no file; was the crate moved?"));
        }
        let text = text.unwrap_or_default();
        let raw = production(&text);
        let code = blank(raw);
        for (name, paren) in extern_fns(raw, &code, true) {
            self.seen += 1;
            n += 1;
            if !ALLOW.contains(&name.as_str()) && !body_of(&code, paren).is_some_and(catches_unwind)
            {
                self.flag(&name, path);
            }
        }
        n
    }
}

/// After a `trampoline!(NAME(..)`: `\s*,?\s*(?:->[^,]*)?,?\s*(\w+)\s*,`, the word.
fn trampoline_kind(after: &str) -> Option<&str> {
    let mut s = after.trim_start();
    s = s.strip_prefix(',').unwrap_or(s).trim_start();
    if let Some(r) = s.strip_prefix("->") {
        s = &r[r.find(',').unwrap_or(r.len())..];
    }
    s = s.strip_prefix(',').unwrap_or(s).trim_start();
    let word = ident(s);
    (!word.is_empty() && s[word.len()..].trim_start().starts_with(',')).then_some(word)
}

pub(super) fn check(tree: &dyn Tree, min_seen: usize) -> Report {
    let mut s = Scan {
        tree,
        bad: Vec::new(),
        seen: 0,
    };
    s.guarded_externs("crates/tutti-clap-ffi/src");
    s.trampolines("crates/tutti-clap-ffi/src");
    s.guarded_externs("crates/tutti-au-ffi/src");
    s.vst3("crates/tutti-vst3-ffi/src/com");
    if s.vst2("crates/tutti-vst2-ffi/src/host.rs") == 0 {
        s.bad.push(
            "no VST2 host callback found in tutti-vst2-ffi/src/host.rs; the scan is broken".into(),
        );
    }
    if s.seen < min_seen {
        s.bad.push(format!(
            "only {} callbacks found; the scan is broken",
            s.seen
        ));
    }
    let mut r = Report::default();
    if s.bad.is_empty() {
        r.say(format!(
            "check-guards: {} callbacks, all guarded or allow-listed",
            s.seen
        ));
    } else {
        r.fail("check-guards: FAIL");
        for b in s.bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const CLAP: &str = "crates/tutti-clap-ffi/src/host.rs";
    const AU: &str = "crates/tutti-au-ffi/src/lib.rs";
    const VST3: &str = "crates/tutti-vst3-ffi/src/com/host.rs";
    const VST2: &str = "crates/tutti-vst2-ffi/src/host.rs";

    const CLEAN: &[(&str, &str)] = &[
        (
            CLAP,
            "extern \"C\" fn host_request_restart(h: *const H) {\n    guarded(h, (), |cb| cb.restart())\n}\n\
             unsafe extern \"C\" fn host_params_clear(h: *const H) {}\n\
             extern \"C\" fn typed(h: *const H) { guarded::<u8>(h, 0, |cb| 1) }\n\
             trampoline!(host_request_process(), guarded, (), |cb| cb.p());\n\
             trampoline!(host_thread_check_is_main() -> bool, guarded, false, |cb| cb.m());\n\
             trampoline!(host_latency_changed(), main_only, (), |cb| cb.l());\n\
             trampoline!(\n    host_request_callback(),\n    guarded,\n    (),\n    |cb| cb.c()\n);\n\
             // extern \"C\" fn in_a_comment() {}\n\
             #[cfg(test)]\nmod tests {\n    extern \"C\" fn t() {}\n}\n",
        ),
        (AU, "extern \"C\" fn render(a: u8) { guard(|| 1) }\n"),
        (
            VST3,
            "impl IFooTrait for Foo {\n    unsafe fn getEventCount(&self) -> i32 {\n        guarded(|| 1)\n    }\n    unsafe fn createContextMenu(&self) {}\n}\n\
             impl Foo { unsafe fn not_in_a_trait(&self) {} }\n",
        ),
        (
            VST2,
            "unsafe extern \"C\" fn callback_wrapper<T>(a: u8) -> isize {\n    catch_unwind(|| 1).unwrap_or(0)\n}\n",
        ),
    ];

    fn run(edit: &[(&str, &str)]) -> Report {
        let mut m = Mem::with(CLEAN);
        for (p, t) in edit {
            m.files.insert((*p).to_owned(), (*t).to_owned());
        }
        check(&m, 1)
    }

    #[test]
    fn a_tree_whose_callbacks_are_all_guarded_passes() {
        let r = run(&[]);
        assert!(r.passed(), "{:?}", r.findings);
        assert_eq!(
            r.out,
            ["check-guards: 11 callbacks, all guarded or allow-listed"]
        );
    }

    #[test]
    fn an_unguarded_extern_fn_is_named() {
        let r = run(&[(AU, "extern \"C\" fn render(a: u8) { work(a) }\n")]);
        assert_eq!(
            r.findings[1],
            format!("  unguarded host callback render in {AU}")
        );
        // A guard named only in a comment or a string is not a guard.
        let r = run(&[(
            AU,
            "extern \"C\" fn render(a: u8) { /* guard(x) */ let s = \"guard(\"; }\n",
        )]);
        assert!(!r.passed());
    }

    #[test]
    fn a_trampoline_must_name_the_kind_its_thread_rule_wants() {
        let wrong_main = "trampoline!(host_latency_changed(), guarded, (), |cb| cb.l());\n";
        assert!(!run(&[(CLAP, wrong_main)]).passed());
        let wrong_safe = "trampoline!(host_request_process(), main_only, (), |cb| cb.l());\n";
        assert!(!run(&[(CLAP, wrong_safe)]).passed());
        let unchecked = "trampoline!(host_request_process(), unchecked, (), |cb| cb.l());\n";
        assert_eq!(
            run(&[(CLAP, unchecked)]).findings[1],
            format!("  unguarded host callback host_request_process in {CLAP}")
        );
    }

    #[test]
    fn a_vst3_trait_method_and_the_vst2_callback_are_held() {
        let bare = "impl IFooTrait for Foo {\n    unsafe fn getEventCount(&self) -> i32 { 1 }\n}\n";
        assert_eq!(
            run(&[(VST3, bare)]).findings[1],
            format!("  unguarded host callback getEventCount in {VST3}")
        );
        let bare2 = "unsafe extern \"C\" fn callback_wrapper(a: u8) -> isize { 0 }\n";
        assert_eq!(
            run(&[(VST2, bare2)]).findings[1],
            format!("  unguarded host callback callback_wrapper in {VST2}")
        );
    }

    #[test]
    fn test_code_is_not_scanned_but_a_test_use_above_production_code_does_not_end_the_scan() {
        let src = "#[cfg(test)]\nuse std::fmt;\nextern \"C\" fn late(a: u8) { work() }\n";
        assert!(!run(&[(AU, src)]).passed());
    }

    #[test]
    fn a_moved_crate_or_an_empty_scan_fails() {
        let mut m = Mem::with(CLEAN);
        m.files.remove(AU);
        assert!(check(&m, 1).findings[1].contains("tutti-au-ffi/src matches no file"));
        assert!(check(&Mem::with(CLEAN), 100).findings[1].contains("only 11 callbacks found"));
    }
}
