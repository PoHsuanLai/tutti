//! RT-safety regression: `AuInstance::process` must not allocate on the
//! audio thread in steady state.
//!
//! Mirrors `tutti-clap-host/tests/clap_process_no_alloc.rs` and
//! `tutti-vst3-host/tests/vst3_process_no_alloc.rs`. macOS-only; uses
//! Apple's stock AUDelay (always installed), so the test runs anywhere
//! the AU framework is available — no third-party plugin required.
//! Still `#[ignore]`'d to match the other host-level rt tests:
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_process_no_alloc -- --ignored
//! ```

#![cfg(target_os = "macos")]

use std::sync::Mutex;

use assert_no_alloc::AllocDisabler;
use tutti_au_host::component::{enumerate_components_of_type, AuType};
use tutti_au_host::instance::AuInstance;

// The `assert_no_alloc` checks below are inert unless `AllocDisabler` is the
// active global allocator for THIS test binary. The `#[cfg(test)]` decl in
// `src/lib.rs` does not apply to integration tests, so declare it here —
// matching `tutti-vst3-host` / `tutti-clap-host`.
#[global_allocator]
static A: AllocDisabler = AllocDisabler;

static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

/// Load Apple's AUDelay, or panic.
///
/// Absence is a hard failure rather than a skip: AUDelay ships with macOS, so
/// not finding it means the AU environment is broken, not that an optional
/// plugin is missing. This used to `return` early with an `eprintln!`, which
/// the harness still counts as a pass — the same silent-skip shape that once
/// let 31 of 32 VST3 conformance tests report `ok` having run nothing.
///
/// Matching is by component code rather than display-name substring: names are
/// localized and have been renamed across releases, and a substring match would
/// happily bind to a third-party unit. Mirrors `tests/support/corpus.rs`.
fn load_apple_delay() -> AuInstance {
    let wanted = u32::from_be_bytes(*b"dely");
    let apple = u32::from_be_bytes(*b"appl");
    let info = enumerate_components_of_type(AuType::Effect)
        .into_iter()
        .find(|i| i.sub_type == wanted && i.manufacturer_code == apple)
        .expect(
            "Apple's AUDelay is not registered with AudioToolbox. It ships \
             with macOS, so this means the AU environment is broken.",
        );
    // SAFETY: `info.component` was returned by AudioComponentFindNext under
    // an Apple-owned AUDelay description; valid for the lifetime of the host
    // process.
    let mut au =
        unsafe { AuInstance::new(info.component, 48_000.0, 64) }.expect("instantiate AUDelay");
    au.initialize().expect("initialize AUDelay");
    au
}

/// Drive `iters` silent blocks, asserting each one actually rendered.
///
/// The render result was previously discarded with `let _`, which made the
/// whole no-alloc assertion vacuous: a `process` that failed on every call
/// allocates nothing, so the test passed just as readily when no audio was
/// being produced at all.
///
/// The `expect` is not itself an allocation risk inside the guarded section —
/// it only formats on the failure path, and a failure fails the test anyway.
fn drive_silent(au: &mut AuInstance, iters: usize) {
    let in_l = [0.0f32; 64];
    let in_r = [0.0f32; 64];
    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];
    for _ in 0..iters {
        let ins: &[&[f32]] = &[&in_l, &in_r];
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        au.process(ins, outs, 64).expect("steady-state render");
    }
}

#[test]
#[ignore]
fn process_steady_state_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut au = load_apple_delay();

    // Warm-up: any first-call lazy allocations inside the AU and the host
    // adapter settle.
    drive_silent(&mut au, 32);

    assert_no_alloc::assert_no_alloc(|| {
        drive_silent(&mut au, 256);
    });
}
