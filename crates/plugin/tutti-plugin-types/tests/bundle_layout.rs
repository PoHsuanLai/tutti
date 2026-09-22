//! The bundle walk, exercised from outside the crate against real directories.
//!
//! `bundle`'s failure mode is silence — a walk that looks in the wrong place
//! returns `None`, which is the same answer as "this is not a bundle" — so the
//! only coverage worth having is a bundle assembled on disk and probed. Every
//! test here builds one with `tempfile`; faking the filesystem would test the
//! fake, and the layout is the thing under test.
//!
//! # What the in-crate unit tests already cover, and what is left
//!
//! `src/bundle.rs`'s eleven `#[test]`s were read first. They cover, and are
//! deliberately **not** repeated here:
//!
//! - `every_native_subdir_is_one_the_all_list_knows` and
//!   `aarch64_linux_and_the_arm64_windows_spellings_are_all_covered` — both are
//!   assertions about the *contents of the two constants*. Neither resolves a
//!   module, so neither notices if a directory is listed but unreachable.
//! - `a_plain_file_is_not_a_bundle` / `a_missing_path_is_not_a_bundle` — a file
//!   and a non-existent path. Neither is a *directory*, which is the input the
//!   `is_file() || !is_dir()` guard lets through to the walk.
//! - `a_foreign_arch_bundle_is_found_by_any_and_not_by_native` — one foreign
//!   directory, chosen by `cfg!`. It shows the two entry points differ; it
//!   does not show that the native one finds anything, nor that every other
//!   directory in the list is refused.
//! - `the_import_library_is_never_taken_for_the_module` and
//!   `a_bundle_of_only_link_byproducts_yields_nothing` — `.lib`/`.exp`/`.pdb`/
//!   `.ilk` beside a module and alone. Fully covered; nothing is added here.
//! - `vst2_and_vst3_disagree_about_the_windows_extension` — one `.dll` bundle
//!   and one `.vst3` bundle, each holding a single file. With one candidate
//!   present, the enumeration fallback reaches the same answer as the extension
//!   table, so the table itself is not what the test pins.
//! - `an_extensionless_binary_resolves_outside_macos_too` (two directories),
//!   `the_outer_extension_on_the_inner_binary_is_accepted` (the `MacOS` bundle
//!   -named spelling) and
//!   `a_named_module_beats_an_enumerated_one_in_an_earlier_directory` (named
//!   lookup precedes enumeration) — the three name spellings `probe_named`
//!   tries, on a sample of directories.
//!
//! The gaps those leave, and the test that fills each:
//!
//! | gap | test |
//! | --- | --- |
//! | a module in each of the fourteen listed directories is actually *resolved*, not merely listed | `every_arch_directory_in_the_list_resolves_its_own_module` |
//! | `native_module_in_bundle` returns `Some` at all, and only for this target's directories | `the_native_walk_finds_this_target_and_refuses_every_other_architecture` |
//! | a directory input — not a bundle, or a bundle with nothing in it | `a_directory_that_holds_no_module_is_none_rather_than_a_panic` |
//! | a *subdirectory* is not a loadable module, however it is named | `a_directory_inside_an_arch_dir_is_never_offered_as_the_module` |
//! | when both extensions are present, the format decides which is the module | `the_format_not_the_directory_listing_picks_between_vst3_and_dll` |
//!
//! Each test's doc comment records the mutation to `src/bundle.rs` that was run
//! to prove the test can fail, per the repo's testing policy. The mutations were
//! applied one at a time and reverted; `bundle.rs` is unmodified.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
// `native_arch_subdirs` is deliberately *not* imported: the one test that cares
// what it returns derives the answer from `std::env::consts` instead, so that a
// wrong entry in it fails a test rather than being copied into the fixture.
// `is_link_byproduct` is left alone too: the two unit tests that put a `.lib`
// beside a module and alone in a directory already pin its effect, and a test
// that called it on a `.lib` would only restate its `matches!`.
use tutti_plugin_types::bundle::{
    any_module_in_bundle, native_module_in_bundle, ModuleKind, ALL_ARCH_SUBDIRS,
};

/// The bundle extension a format wears on the outside.
fn bundle_ext(kind: ModuleKind) -> &'static str {
    match kind {
        ModuleKind::Vst2 => "vst",
        ModuleKind::Vst3 => "vst3",
    }
}

/// What the module inside `arch` is conventionally called, stated here rather
/// than read back from the crate.
///
/// This restates the extension rule independently — the directory name decides
/// it on macOS and Linux, and only on Windows does the *format* decide — so a
/// change to the crate's table shows up as a failure instead of being followed
/// silently by the test.
fn module_name(stem: &str, arch: &str, kind: ModuleKind) -> String {
    if arch == "MacOS" {
        format!("{stem}.dylib")
    } else if arch.ends_with("-linux") {
        format!("{stem}.so")
    } else {
        match kind {
            ModuleKind::Vst2 => format!("{stem}.dll"),
            ModuleKind::Vst3 => format!("{stem}.vst3"),
        }
    }
}

/// Create `Contents/<arch>/` inside `bundle` and return it.
fn arch_dir(bundle: &Path, arch: &str) -> PathBuf {
    let dir = bundle.join("Contents").join(arch);
    fs::create_dir_all(&dir).expect("create arch dir");
    dir
}

fn touch(path: &Path) {
    fs::write(path, b"\x7fELF not really").expect("write module");
}

/// The arch directories *this* running target can load from, derived from
/// `std::env::consts` rather than from the crate's `cfg!` cascade.
///
/// Two independent statements of the same fact are the point: a test that asked
/// `native_arch_subdirs()` which directories to use would keep passing if that
/// function started naming the wrong ones, because it would follow the mistake
/// into the fixture it builds.
fn dirs_this_target_can_load() -> Vec<String> {
    let arch = std::env::consts::ARCH;
    match (std::env::consts::OS, arch) {
        // A macOS bundle binary is fat; there is no per-arch directory.
        ("macos" | "ios", _) => vec!["MacOS".to_string()],
        ("linux", "x86") => vec!["i686-linux".to_string(), "i386-linux".to_string()],
        ("linux", "arm") => vec![
            "armv7l-linux".to_string(),
            "armv8l-linux".to_string(),
            "arm-linux".to_string(),
        ],
        ("linux", other) => vec![format!("{other}-linux")],
        // ARM64 Windows loads arm64ec/arm64x natively and x64 under emulation.
        ("windows", "aarch64") => vec![
            "arm64-win".to_string(),
            "arm64ec-win".to_string(),
            "arm64x-win".to_string(),
            "x86_64-win".to_string(),
        ],
        ("windows", "x86") => vec!["x86-win".to_string()],
        ("windows", "arm") => vec!["arm-win".to_string(), "x86-win".to_string()],
        ("windows", other) => vec![format!("{other}-win")],
        _ => vec!["MacOS".to_string()],
    }
}

/// Every directory the crate offers a corpus scan must actually yield the
/// module a bundle conventionally puts there.
///
/// The two unit tests that mention the arch list only assert what the constant
/// *contains*. That is one half of the divergence this module was written to
/// end; the other half is the extension table, which is keyed on the directory
/// name and has to agree with it — a directory that is listed but whose
/// extension is wrong is listed and unreachable, and reports as "not a bundle".
///
/// Each fixture carries a decoy file sorting ahead of the module in the same
/// directory. Without it the enumeration fallback would return the module
/// anyway — it is the only other file there — and the test would pass for a
/// reason that has nothing to do with the named lookup it means to check.
///
/// Mutation run: in `exts_for`, `if arch_dir.ends_with("-linux") { return
/// &["so"] }` changed to `return &["dll"]`. The six `-linux` cases then resolved
/// `0-decoy` instead of the module and the test failed; reverted.
#[test]
fn every_arch_directory_in_the_list_resolves_its_own_module() {
    let tmp = TempDir::new().expect("tmp");
    for kind in [ModuleKind::Vst2, ModuleKind::Vst3] {
        for arch in ALL_ARCH_SUBDIRS {
            let stem = format!("plug-{arch}");
            let bundle = tmp.path().join(format!("{stem}.{}", bundle_ext(kind)));
            let dir = arch_dir(&bundle, arch);
            let module = dir.join(module_name(&stem, arch, kind));
            touch(&module);
            touch(&dir.join("0-decoy"));

            assert_eq!(
                any_module_in_bundle(&bundle, kind).as_deref(),
                Some(module.as_path()),
                "a {kind:?} module in Contents/{arch} was not resolved by name; a \
                 bundle built for that architecture reads as no bundle at all"
            );
        }
    }
}

/// The native walk must find this target's module, and must refuse every other
/// architecture's.
///
/// The existing unit test shows one foreign directory being refused, which
/// proves the two entry points differ but never asserts that the native one
/// resolves anything: `native_arch_subdirs()` could name a directory no host
/// writes and every test in the crate would still pass, while every real host
/// reported every real plugin missing. The negative half is the same property
/// from the other side — "native" has to mean *only* native, or the loader is
/// handed a binary it cannot map and the honest "no module for this
/// architecture" answer turns into a load error.
///
/// Mutations run, one at a time, both in `native_arch_subdirs`'s
/// `linux`/`x86_64` arm. `&["x86_64-linux"]` changed to `&["aarch64-linux"]`:
/// the positive half failed, with no module found for this target. Changed to
/// `&["x86_64-linux", "MacOS"]`: the negative half failed, with a `MacOS`
/// binary offered to a Linux loader. Both reverted.
#[test]
fn the_native_walk_finds_this_target_and_refuses_every_other_architecture() {
    let tmp = TempDir::new().expect("tmp");
    let native = dirs_this_target_can_load();
    let kind = ModuleKind::Vst3;

    for arch in ALL_ARCH_SUBDIRS {
        let stem = format!("host-{arch}");
        let bundle = tmp.path().join(format!("{stem}.vst3"));
        let dir = arch_dir(&bundle, arch);
        let module = dir.join(module_name(&stem, arch, kind));
        touch(&module);

        let got = native_module_in_bundle(&bundle, kind);
        if native.iter().any(|n| n == arch) {
            assert_eq!(
                got.as_deref(),
                Some(module.as_path()),
                "Contents/{arch} is a directory {}/{} loads from, and the host \
                 walk missed it — every plugin built for this machine would \
                 report as missing",
                std::env::consts::OS,
                std::env::consts::ARCH,
            );
        } else {
            assert_eq!(
                got, None,
                "Contents/{arch} holds a binary this target cannot map, and the \
                 host walk offered it to the loader anyway"
            );
        }
    }
}

/// A directory that is not a populated bundle answers `None`, and does not
/// panic doing it.
///
/// The unit tests reject a plain file and a path that does not exist — both
/// turned away by the `is_file() || !is_dir()` guard before any walking
/// happens. A *directory* is the input that gets past that guard, and the three
/// shapes here are the ones a scanner actually meets: a plain folder in a
/// plugin search path, a bundle whose `Contents` was never populated, and an
/// arch directory that exists but is empty (what a failed build leaves). All
/// three reach `read_dir` on paths that may not exist, so "no module" has to
/// come back as `None` rather than as an unwrap on a missing directory — a
/// panic inside a corpus scan takes the whole scan down instead of skipping one
/// entry.
///
/// Mutations run, one at a time, both in `probe_dirs`'s enumeration. `let
/// Ok(entries) = std::fs::read_dir(&dir) else { continue };` changed to `let
/// entries = std::fs::read_dir(&dir).unwrap();`: the walk panicked on the
/// absent arch directory instead of answering `None`. `let dir =
/// bundle.join("Contents").join(sub);` changed to `let dir =
/// bundle.to_path_buf();`: the plain folder resolved its `readme.txt` as a
/// module, which is the wrong-`Some` half of the same property. Both reverted.
#[test]
fn a_directory_that_holds_no_module_is_none_rather_than_a_panic() {
    let tmp = TempDir::new().expect("tmp");

    let not_a_bundle = tmp.path().join("just-a-folder");
    fs::create_dir_all(&not_a_bundle).expect("mkdir");
    touch(&not_a_bundle.join("readme.txt"));

    let empty_contents = tmp.path().join("hollow.vst3");
    fs::create_dir_all(empty_contents.join("Contents")).expect("mkdir");

    let empty_arch = tmp.path().join("unbuilt.vst3");
    arch_dir(&empty_arch, "x86_64-linux");

    for candidate in [&not_a_bundle, &empty_contents, &empty_arch] {
        for kind in [ModuleKind::Vst2, ModuleKind::Vst3] {
            assert_eq!(
                any_module_in_bundle(candidate, kind),
                None,
                "{candidate:?} holds no module, so a corpus scan must get `None`"
            );
            assert_eq!(
                native_module_in_bundle(candidate, kind),
                None,
                "{candidate:?} holds no module, so a host must get `None`"
            );
        }
    }
}

/// A subdirectory is never the module, whatever it is called.
///
/// Both halves of the walk have to check this and neither is covered: the named
/// lookup because a bundle may nest another bundle under the very name it
/// probes for — the bare stem is the first spelling it tries, and on macOS a
/// helper or a nested `.framework` sits right there — and the enumeration
/// because `read_dir` yields directories alongside files. Handing either to
/// `dlopen`/`LoadLibrary` fails the load, which reports as a corrupt plugin:
/// the same wrong diagnosis the `.lib` mix-up produced, from a different cause.
///
/// The second half then puts a real module in that same directory, so the test
/// distinguishes "skipped the directories" from "found nothing at all".
///
/// Mutations run, one at a time, each failing at the first assertion. In
/// `probe_named`, `if bare.is_file()` changed to `if bare.exists()`: the
/// walk returned the nested `nested/` directory. In `probe_dirs`, the
/// enumeration filter `p.is_file() && !is_link_byproduct(p)` changed to
/// `!is_link_byproduct(p)`: it returned `0-helpers/`, the directory sorting
/// first. Both reverted.
#[test]
fn a_directory_inside_an_arch_dir_is_never_offered_as_the_module() {
    let tmp = TempDir::new().expect("tmp");
    let bundle = tmp.path().join("nested.vst3");
    let dir = arch_dir(&bundle, "x86_64-linux");
    // Named exactly what the named lookup probes for first, and a second one
    // that sorts ahead of any module the enumeration might reach.
    fs::create_dir_all(dir.join("nested")).expect("mkdir");
    fs::create_dir_all(dir.join("0-helpers")).expect("mkdir");

    assert_eq!(
        any_module_in_bundle(&bundle, ModuleKind::Vst3),
        None,
        "a directory was offered as a loadable module; the load fails and the \
         plugin is reported corrupt"
    );

    let module = dir.join("nested.so");
    touch(&module);
    assert_eq!(
        any_module_in_bundle(&bundle, ModuleKind::Vst3).as_deref(),
        Some(module.as_path()),
        "the real module beside those directories was not found"
    );
}

/// When a Windows bundle ships both spellings, the *format* picks, not whatever
/// `read_dir` hands over first.
///
/// This is the half of the VST2/VST3 extension split the unit test cannot see:
/// it gives each bundle a single candidate, and with one file present the
/// enumeration fallback reaches the same answer as the extension table, so the
/// table could be empty and the test would still pass. A real Windows bundle
/// carrying both is ordinary — the VST3 SDK's own modules were `.dll` before
/// they were `.vst3`, and a bundle built for both eras holds the two side by
/// side — and picking the wrong one loads a module whose entry point the host
/// is not looking for.
///
/// Mutation run: in `exts_for`, the `ModuleKind::Vst3` arm reordered from
/// `&["vst3", "dll"]` to `&["dll", "vst3"]`. The VST3 half resolved `p.dll` and
/// failed; reverted. The VST2 arm was separately changed from `&["dll"]` to
/// `&["vst3", "dll"]`, which failed the VST2 half; reverted.
#[test]
fn the_format_not_the_directory_listing_picks_between_vst3_and_dll() {
    let tmp = TempDir::new().expect("tmp");

    let vst3 = tmp.path().join("p.vst3");
    let vst3_dir = arch_dir(&vst3, "x86_64-win");
    touch(&vst3_dir.join("p.dll"));
    touch(&vst3_dir.join("p.vst3"));
    assert_eq!(
        any_module_in_bundle(&vst3, ModuleKind::Vst3).as_deref(),
        Some(vst3_dir.join("p.vst3").as_path()),
        "a VST3 bundle holding both spellings resolved the legacy `.dll`"
    );

    let vst2 = tmp.path().join("q.vst");
    let vst2_dir = arch_dir(&vst2, "x86_64-win");
    touch(&vst2_dir.join("q.dll"));
    touch(&vst2_dir.join("q.vst3"));
    assert_eq!(
        any_module_in_bundle(&vst2, ModuleKind::Vst2).as_deref(),
        Some(vst2_dir.join("q.dll").as_path()),
        "a VST2 bundle resolved a `.vst3`, which no VST2 host can enter"
    );
}
