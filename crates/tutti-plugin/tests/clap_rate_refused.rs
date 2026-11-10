//! **A plugin that refuses the engine's rate is silent, visibly, and retried.**
//!
//! The graph re-prepares the node at a new rate and the host tells the plugin.
//! A plugin that refuses (CLAP `activate` returns false) rolls back to the
//! rate it had, or stays deactivated, while the host's blocks and transport
//! are at the new rate: left running it would sound pitch-shifted. Instead
//! the server renders silence for it, the host reports
//! `PluginStatus::RateRefused` (not `Dead`), and every later rate change
//! tries it again; one it takes clears the error and the plugin renders.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{ProbeEnv, Rig, load_probe, render};
use tutti_fixtures::machine_lock;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tutti_graph::Prepare;
use tutti_plugin::handles::{PluginHandle, PluginInvalidation, PluginStatus};
use tutti_types::{SampleRate, Samples};

/// The rate the plugin is loaded at, and accepts.
const LOAD_RATE: f64 = 48_000.0;
/// The rate the probe refuses.
const REFUSED_RATE: f64 = 96_000.0;
/// Another rate the probe accepts.
const OTHER_RATE: f64 = 44_100.0;
const BLOCK: usize = 64;
/// Long enough for the subprocess to answer each block before the next: the
/// pipeline never waits for a reply, so a tight loop reads as silence.
const PACE: Duration = Duration::from_millis(30);
const NOTICE_TIMEOUT: Duration = Duration::from_secs(10);

/// A value neither silence nor anything the probe adds to it.
const INPUT_DC: f32 = 7.0;
/// `INPUT_DC` plus the probe's `TagPassthrough` tag on port 0, channel 0.
const LIVE: f32 = INPUT_DC + 1.0;

fn drive_block(unit: &mut Rig) -> Vec<f32> {
    let out = unit.run(BLOCK, |_, _| INPUT_DC);
    std::thread::sleep(PACE);
    out.into_iter().next().expect("the probe has outputs")
}

fn reprepare(unit: &mut Rig, rate: f64) {
    unit.renderer()
        .editor_mut()
        .reprepare(Prepare::new(SampleRate(rate), Samples(BLOCK)))
        .expect("the graph re-prepares");
}

/// Drive blocks (a re-prepare reaches the node at the next block) until
/// `want` holds of the status, or the budget runs out.
fn wait_for(
    unit: &mut Rig,
    handle: &PluginHandle,
    mut want: impl FnMut(&PluginStatus) -> bool,
) -> PluginStatus {
    let deadline = Instant::now() + NOTICE_TIMEOUT;
    loop {
        let _ = drive_block(unit);
        let status = handle.status();
        if want(&status) || Instant::now() >= deadline {
            return status;
        }
    }
}

fn refused_at(status: &PluginStatus, rate: f64) -> bool {
    matches!(status, PluginStatus::RateRefused { rate: r, reason } if r.0 == rate && !reason.is_empty())
}

/// Drive until `blocks` consecutive blocks are live, within a bounded budget.
fn renders_live(unit: &mut Rig) -> bool {
    let mut live = 0;
    for _ in 0..60 {
        if drive_block(unit).iter().all(|&s| s == LIVE) {
            live += 1;
            if live >= 2 {
                return true;
            }
        } else {
            live = 0;
        }
    }
    false
}

fn assert_silent(unit: &mut Rig, what: &str) {
    for block in 0..4 {
        let out = drive_block(unit);
        assert!(
            out.iter().all(|&s| s == 0.0),
            "{what}: block {block} is silent, not the plugin at the wrong rate: {out:?}"
        );
    }
}

/// (The dropout count is pinned inside the cycle below.)
///
/// The whole cycle on one plugin: running, refused (silent, visible, not
/// dead, an invalidation raised), refused again, revived by a rate it takes,
/// and a drop during a refusal that returns promptly.
///
/// Mutations (run): the bridge latches the refusal as a crash again
/// (`mark_crashed` in the `SampleRateSet` error arm) → `RateRefused` never
/// shows, the status is `Dead` → fails. The server's `rate_refused` early
/// return removed (`if false && …`) → the rolled-back plugin renders at its
/// old rate and the silence assertion fails. The bridge never clears the
/// refusal on a success reply → the status stays `RateRefused` after the
/// accepted rate → fails. The server keeps `rate_refused` after a success →
/// the plugin stays silent → fails. `collect` counts a missed chunk while the
/// refusal is latched (drop the `rate_refusal().is_none()` condition) → the
/// dropout count rises → fails.
#[test]
fn a_refused_rate_is_silent_visible_and_retried() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_PASSTHROUGH)
        .refuse_rate(REFUSED_RATE);
    let probe = load_probe(LOAD_RATE);
    let handle = probe.handle.clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    handle.on_invalidate(move |e| sink.lock().unwrap().push(e));

    let mut unit = Rig::new(probe.client.bind(), LOAD_RATE, BLOCK);
    assert!(renders_live(&mut unit), "the plugin renders at {LOAD_RATE}");
    assert_eq!(handle.status(), PluginStatus::Alive);

    // The graph moves to a rate the plugin refuses.
    reprepare(&mut unit, REFUSED_RATE);
    let status = wait_for(&mut unit, &handle, |s| *s != PluginStatus::Alive);
    assert!(refused_at(&status, REFUSED_RATE), "{status:?}");
    assert!(!status.is_dead() && !handle.is_crashed());
    let missed = unit.controls.missed_chunks();
    assert_silent(&mut unit, "refused");
    assert_eq!(
        unit.controls.missed_chunks(),
        missed,
        "a refused plugin's silence is deliberate, not a dropout"
    );
    assert!(
        events.lock().unwrap().iter().any(
            |e| matches!(e, PluginInvalidation::RateRefused { rate, .. } if rate.0 == REFUSED_RATE)
        ),
        "{:?}",
        events.lock().unwrap()
    );

    // Asked again at the same rate: refused again, still silent.
    reprepare(&mut unit, REFUSED_RATE);
    std::thread::sleep(Duration::from_millis(200));
    assert!(refused_at(&handle.status(), REFUSED_RATE));
    assert_silent(&mut unit, "refused again");

    // A rate the plugin takes brings it back.
    reprepare(&mut unit, OTHER_RATE);
    let status = wait_for(&mut unit, &handle, |s| *s == PluginStatus::Alive);
    assert_eq!(status, PluginStatus::Alive, "the error clears");
    assert!(renders_live(&mut unit), "the plugin renders again");
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PluginInvalidation::RateRestored)),
        "{:?}",
        events.lock().unwrap()
    );

    // Refused once more, then dropped: teardown does not wait out anything.
    reprepare(&mut unit, REFUSED_RATE);
    let status = wait_for(&mut unit, &handle, |s| *s != PluginStatus::Alive);
    assert!(refused_at(&status, REFUSED_RATE), "{status:?}");
    let started = Instant::now();
    drop(unit);
    drop(probe.handle);
    drop(handle);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "dropping a refused plugin took {:?}",
        started.elapsed()
    );
}
