//! When does this host read a VST2 plugin's latency, and is that soon enough?
//!
//! `AEffect::initialDelay` is the whole of VST 2.4's latency reporting — there
//! is no opcode to ask for it and no callback to announce a change. So *when*
//! the host reads the field is the entire behaviour.
//!
//! The vendored `PluginInstance::new` snapshots the `AEffect` into an `Info`
//! before `effOpen`, `effSetSampleRate` or `effMainsChanged` have run, and
//! `get_info()` hands back a clone of that snapshot. Reading latency from it
//! catches only plugins that declare at construction — while the plugins that
//! *have* latency (linear-phase EQ, look-ahead limiter, oversampling anything)
//! cannot know their filter length until they know the sample rate, so they
//! declare during `effSetSampleRate` and read back as 0.
//!
//! The probe models exactly that: `tutti_vst2_probe_set_late_latency` makes it
//! report a figure only from `effSetSampleRate` onwards. A probe that declared
//! up front could not distinguish a host that re-reads from one that does not —
//! both would see the same number, and the suite would pass either way.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use tutti_vst2_host::{Samples, Vst2Instance};

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 44_100.0;
const BLOCK: usize = 512;

/// Latency the probe declares once it has been told its sample rate.
///
/// Not a round number, so a value that happens to match cannot be a default,
/// a block size, or a channel count read by mistake.
const LATE_LATENCY: i32 = 1537;

/// Serializes the probe's process-global switches across load→assert. Shared
/// image, shared statics — the same reason the other suites have one.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// Reach a `#[no_mangle]` switch in the probe.
///
/// The probe is a cdylib the host `dlopen`s at run time, not something this
/// test binary links against, so its exports are resolved by re-opening the
/// same file rather than declared `extern "C"` here.
fn set_late_latency(samples: i32) {
    let path = Path::new(probe_path::probe_path());
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: the probe exports this as `extern "C" fn(i32)`.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(b"tutti_vst2_probe_set_late_latency\0") }
            .expect("probe missing symbol tutti_vst2_probe_set_late_latency");
    let f: extern "C" fn(i32) = unsafe { std::mem::transmute(*sym) };
    f(samples);
}

/// Holds the lock and clears the switch on drop, so a failing assertion cannot
/// leave a latency-declaring probe behind for the next suite in this binary.
struct Probe {
    _lock: MutexGuard<'static, ()>,
}

impl Probe {
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_late_latency(0);
        Probe { _lock: lock }
    }

    /// Make the probe declare `samples` from `effSetSampleRate` onwards.
    fn declares_late(&self, samples: i32) {
        set_late_latency(samples);
    }

    fn load(&self) -> Vst2Instance {
        Vst2Instance::load(probe_path::probe_path(), SAMPLE_RATE, BLOCK)
            .expect("the probe must load")
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        set_late_latency(0);
    }
}

/// Latency declared during `effSetSampleRate` reaches the load-time metadata.
///
/// This is D-4. The host built `PluginInfo.latency_samples` from
/// `get_info().initial_delay` — the pre-`effOpen` snapshot — so a plugin
/// declaring here reported 0, and PDC compensated nothing for exactly the
/// plugins that need it.
#[test]
fn latency_declared_after_construction_reaches_the_metadata() {
    let probe = Probe::acquire();
    probe.declares_late(LATE_LATENCY);

    let instance = probe.load();

    assert_eq!(
        instance.metadata().latency_samples,
        Samples(LATE_LATENCY as usize),
        "the host read `initialDelay` from the construction-time snapshot, \
         before the plugin had been told its sample rate"
    );
}

/// A plugin that declares no latency still reports none.
///
/// The negative half: re-reading must not manufacture a figure. Without this a
/// host that always reported some constant would pass the test above.
#[test]
fn a_plugin_with_no_latency_reports_none() {
    let probe = Probe::acquire();
    // No `declares_late` call — the probe's default.

    let instance = probe.load();

    assert_eq!(
        instance.metadata().latency_samples,
        Samples(0),
        "a probe declaring no latency must not acquire one"
    );
}

/// `latency()` re-reads the live `AEffect` rather than replaying the snapshot.
///
/// The accessor exists because VST2 has no latency-changed callback: a plugin
/// may alter `initialDelay` on a sample-rate change and tell nobody, so the
/// host has to ask again. Agreeing with the metadata here is what shows both
/// read the same live field.
#[test]
fn the_latency_accessor_reads_the_live_field() {
    let probe = Probe::acquire();
    probe.declares_late(LATE_LATENCY);

    let instance = probe.load();

    assert_eq!(
        instance.latency(),
        Samples(LATE_LATENCY as usize),
        "the accessor must see what the plugin declared during init"
    );
    assert_eq!(
        instance.latency(),
        instance.metadata().latency_samples,
        "the accessor and the metadata read the same field"
    );
}

/// A sample-rate change re-asks, and picks up a figure the plugin recomputed.
///
/// The "never refreshed" half of the finding. `set_sample_rate` suspends and
/// resumes, which is precisely when a plugin resizes a rate-dependent filter;
/// the host has no callback to learn that, so a caller re-reads through
/// [`Vst2Instance::latency`].
///
/// The probe recomputes on every `effSetSampleRate`, so switching the declared
/// figure between the load and the rate change is what makes the re-read
/// observable — a probe with a fixed figure would return the same number
/// whether or not the host asked again.
#[test]
fn a_sample_rate_change_can_be_followed_by_a_fresh_read() {
    let probe = Probe::acquire();
    probe.declares_late(LATE_LATENCY);

    let mut instance = probe.load();
    assert_eq!(instance.latency(), Samples(LATE_LATENCY as usize));

    // A plugin that halves its filter length at a higher rate.
    let recomputed = LATE_LATENCY / 2;
    probe.declares_late(recomputed);
    instance.set_sample_rate(96_000.0);

    assert_eq!(
        instance.latency(),
        Samples(recomputed as usize),
        "after a rate change the host must re-ask; the load-time figure is stale"
    );
}
