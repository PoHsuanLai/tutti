//! **A plugin that cannot run at the engine's rate is dead to the host.**
//!
//! The engine re-prepares a node at the device's rate and the host tells the
//! plugin. A plugin that refuses (CLAP `activate` returns false) rolls back to
//! the rate it had or stays deactivated, while the host's blocks and transport
//! are already at the new rate: it would render pitch-shifted audio or
//! nothing. The server answers the rate change, the bridge latches the refusal
//! as the crash cause (`PluginStatus::Dead`), and the node renders silence,
//! the same path a plugin that aborted takes.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{exclusive, load_probe, render, ProbeEnv, Rig};

use std::time::{Duration, Instant};

use tutti_plugin::handles::{PluginHandle, PluginStatus};

/// The rate the plugin is loaded at, and accepts.
const LOAD_RATE: f64 = 48_000.0;
/// The rate the probe refuses.
const REFUSED_RATE: f64 = 96_000.0;
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

fn wait_for_death(handle: &PluginHandle) -> PluginStatus {
    let deadline = Instant::now() + NOTICE_TIMEOUT;
    loop {
        let status = handle.status();
        if status.is_dead() || Instant::now() >= deadline {
            return status;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The node is prepared at a rate the plugin refuses: the host learns it as a
/// dead plugin whose cause names the rate, and the node renders silence
/// rather than the plugin at the wrong rate.
///
/// Mutation (run): the bridge ignores the reply (`SampleRateSet` with an
/// error `break`s instead of returning `SampleRateRefused`) → the status
/// stays alive, the wait times out → fails. Make the server answer `error:
/// None` after a refusal → same.
#[test]
fn a_refused_rate_kills_the_plugin_and_silences_the_node() {
    let _lock = exclusive();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_PASSTHROUGH)
        .refuse_rate(REFUSED_RATE);
    let probe = load_probe(LOAD_RATE);
    let handle = probe.handle.clone();
    assert!(
        !handle.status().is_dead(),
        "the load at {LOAD_RATE} is accepted"
    );

    // The graph prepares the node at the refused rate, as a device switch
    // would.
    let mut unit = Rig::new(probe.client.bind(), REFUSED_RATE, BLOCK);

    let status = wait_for_death(&handle);
    let cause = status.cause().unwrap_or_default().to_owned();
    assert!(
        status.is_dead() && cause.contains("96000"),
        "a plugin that refuses the rate is dead, with the rate in its cause: {status:?}"
    );

    for block in 0..4 {
        let out = drive_block(&mut unit);
        assert!(
            out.iter().all(|&s| s == 0.0),
            "block {block} after the refusal is silent, not the plugin at the \
             wrong rate: {out:?}"
        );
    }
}

/// The control: a rate the plugin accepts leaves it alive and rendering, so
/// the refusal above is the rate's doing and not every rate change's.
///
/// Mutation (run): the bridge treats every `SampleRateSet` as a refusal → the
/// plugin dies at 44.1 kHz → fails.
#[test]
fn an_accepted_rate_leaves_the_plugin_rendering() {
    let _lock = exclusive();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_PASSTHROUGH)
        .refuse_rate(REFUSED_RATE);
    let probe = load_probe(LOAD_RATE);
    let handle = probe.handle.clone();
    let mut unit = Rig::new(probe.client.bind(), 44_100.0, BLOCK);

    let mut live = 0;
    for _ in 0..40 {
        if drive_block(&mut unit).iter().all(|&s| s == LIVE) {
            live += 1;
            if live >= 2 {
                break;
            }
        }
    }
    assert!(live >= 2, "the plugin renders at an accepted rate");
    assert!(!handle.status().is_dead(), "{:?}", handle.status());
}
