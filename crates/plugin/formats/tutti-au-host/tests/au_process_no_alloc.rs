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

fn load_apple_delay_or_skip() -> Option<AuInstance> {
    let effects = enumerate_components_of_type(AuType::Effect);
    let info = effects.into_iter().find(|i| i.name.contains("AUDelay"))?;
    // SAFETY: `info.component` was returned by AudioComponentFindNext under
    // an Apple-owned AUDelay description; valid for the lifetime of the host
    // process.
    let mut au = unsafe { AuInstance::new(info.component, 48_000.0, 64) }.ok()?;
    au.initialize().ok()?;
    Some(au)
}

fn drive_silent(au: &mut AuInstance, iters: usize) {
    let in_l = [0.0f32; 64];
    let in_r = [0.0f32; 64];
    let mut out_l = [0.0f32; 64];
    let mut out_r = [0.0f32; 64];
    for _ in 0..iters {
        let ins: &[&[f32]] = &[&in_l, &in_r];
        let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
        let _ = au.process(ins, outs, 64);
    }
}

#[test]
#[ignore]
fn process_steady_state_does_not_allocate() {
    let _lock = PLUGIN_LOAD_LOCK.lock().unwrap();
    let Some(mut au) = load_apple_delay_or_skip() else {
        eprintln!("AUDelay not available, skipping");
        return;
    };

    // Warm-up: any first-call lazy allocations inside the AU and the host
    // adapter settle.
    drive_silent(&mut au, 32);

    assert_no_alloc::assert_no_alloc(|| {
        drive_silent(&mut au, 256);
    });
}
