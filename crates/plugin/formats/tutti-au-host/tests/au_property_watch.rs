//! Does a *runtime* change to latency, tail or the parameter list reach a host
//! that is watching for it?
//!
//! Latency and tail are `Float64` **seconds** properties an AU may rewrite at
//! any time. A linear-phase EQ switching modes, an oversampling toggle, a reverb
//! whose decay is turned up — each moves a figure a host read once at load and
//! then compensates or sizes a bounce from forever. The parameter list is the
//! third: a unit that grows or renames parameters leaves a cached range table
//! denormalizing automation against bounds that are no longer declared.
//!
//! `listener.rs` has carried the mechanism for all three the whole time —
//! `watch_property` on any `kAudioUnitProperty_*` id — and until now nothing
//! outside a test constructed an `AuParameterListener`. `au_api_surface.rs`
//! proves *delivery* works, against `BypassEffect` and `Latency`. What it cannot
//! prove is that a change to one of these three specific properties arrives,
//! because no installed AU changes any of them on request:
//!
//! * every Apple unit's latency is fixed for the life of the instance
//! * tail moves only with a decay parameter, on units that report tail at all
//! * no unit adds or removes parameters after load
//!
//! So the fixture is the probe, which stores the property listeners the host
//! installs and posts a notification when a test moves one of the three values.
//! A probe that discarded them would make every assertion here vacuous. See
//! `support/probe_au.rs`.
//!
//! # What this does not cover
//!
//! The subprocess half. `tutti-plugin-server`'s AU loader installs one of these
//! listeners and turns a raised flag into an `AsyncEvent` that crosses IPC; that
//! wiring is pinned by
//! `tutti_plugin_server::plugin::tests::an_au_property_change_reaches_the_event_list`,
//! in the crate that owns it. The split is deliberate — this file owns "the AU
//! told us", that one owns "the host passed it on".
//!
//! No serialising lock: every unit here is a probe with its own subtype code and
//! its own instance state, as `au_misbehaving.rs` explains.
//!
//! # Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_property_watch
//! ```

#![cfg(target_os = "macos")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tutti_au_host::listener::{AuEvent, AuParameterListener, EventAddress};
use tutti_au_host::types::{
    K_AUDIO_UNIT_PROPERTY_LATENCY, K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST,
    K_AUDIO_UNIT_PROPERTY_TAIL_TIME,
};
use tutti_types::Samples;

mod support;
use support::probe_au::{
    set_latency_seconds, set_param_count, set_tail_seconds, Misbehaviour,
    PROBE_INITIAL_PARAM_COUNT, PROBE_INITIAL_TAIL_SECONDS,
};

const BLOCK: u32 = 64;
const RATE: f64 = 48_000.0;

/// How long to wait for a notification before calling the path dead.
///
/// `au_api_surface.rs` measured first arrival at 202–216 ms against Apple units,
/// which is the listener's own 200 ms `NOTIFICATION_INTERVAL` plus delivery.
/// A second is comfortably clear of that without turning a genuine failure into
/// a minute-long hang.
const SETTLE: Duration = Duration::from_secs(1);

/// Every `PropertyChanged` id the listener delivered, oldest first.
#[derive(Default)]
struct Log {
    ids: Mutex<Vec<u32>>,
    count: AtomicUsize,
}

impl Log {
    fn sink(self: &Arc<Self>) -> impl Fn(AuEvent) + Send + Sync + 'static {
        let me = Arc::clone(self);
        move |ev| {
            if let AuEvent::PropertyChanged { id, .. } = ev {
                me.ids.lock().unwrap().push(id);
                me.count.fetch_add(1, Ordering::Release);
            }
        }
    }

    /// Block until `id` has been delivered, or [`SETTLE`] elapses.
    fn wait_for(&self, id: u32) -> bool {
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            if self.ids.lock().unwrap().contains(&id) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn snapshot(&self) -> Vec<u32> {
        self.ids.lock().unwrap().clone()
    }
}

/// A probe watched on all three properties.
///
/// A struct rather than a tuple so the **drop order is declared here**, once,
/// instead of depending on each test remembering to drop the listener by hand.
/// `AuParameterListener`'s safety contract is that the `AudioUnit` outlives the
/// registration, and Rust drops fields in declaration order — so `listener` is
/// declared before `au`. A tuple return would have dropped the instance first,
/// leaving AudioToolbox a registration pointing at a disposed unit.
struct Watched {
    /// Held for its `Drop`, which runs `AUListenerDispose`. Never read — the
    /// registration does its work through AudioToolbox, not through this field —
    /// so the lint is silenced rather than the field removed: removing it is
    /// exactly the bug it prevents.
    #[allow(dead_code)]
    listener: Option<AuParameterListener>,
    au: tutti_au_host::instance::AuInstance,
    log: Arc<Log>,
}

impl Watched {
    fn new() -> Self {
        let au = Misbehaviour::None.open_initialized(RATE, BLOCK);
        let log = Arc::new(Log::default());
        // SAFETY: `au` is stored in the same struct as the listener and declared
        // after it, so it outlives the registration.
        let listener = unsafe { AuParameterListener::new(au.raw_unit(), log.sink()) }
            .expect("create a listener on the probe");
        for id in [
            K_AUDIO_UNIT_PROPERTY_LATENCY,
            K_AUDIO_UNIT_PROPERTY_TAIL_TIME,
            K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST,
        ] {
            listener
                .watch_property(id, EventAddress::GLOBAL)
                .unwrap_or_else(|e| panic!("watch_property({id}) failed: {e:?}"));
        }
        Self {
            listener: Some(listener),
            au,
            log,
        }
    }

    /// Dispose the registration while keeping the AU alive — the state
    /// [`a_dropped_listener_stops_receiving`] is about.
    fn drop_listener(&mut self) {
        self.listener = None;
    }
}

/// A latency change must arrive as a `PropertyChanged` carrying the latency id.
///
/// The consequence if it does not: a plugin that changes latency on a mode
/// switch leaves PDC compensating the load-time figure permanently, and nothing
/// anywhere reports it.
#[test]
fn a_latency_change_reaches_a_watching_host() {
    let w = Watched::new();

    // The probe reports 0 s at load, like every non-lying Apple unit. 512 frames
    // at 48 kHz is a figure PDC would have to act on and that no rounding could
    // turn back into zero.
    set_latency_seconds(&w.au, 512.0 / RATE);

    assert!(
        w.log.wait_for(K_AUDIO_UNIT_PROPERTY_LATENCY),
        "no Latency PropertyChanged arrived within {SETTLE:?}; delivered {:?}",
        w.log.snapshot()
    );

    // The value the host would read on notification must be the new one, not the
    // one that caused it — a notification posted before the write lands would
    // make every re-read report the stale figure and look like nothing changed.
    let latency =
        w.au.get_latency()
            .expect("the probe answers kAudioUnitProperty_Latency");
    assert_eq!(
        latency,
        Samples(512),
        "the AU must already hold the new latency by the time it notifies"
    );
}

/// A tail change must arrive under the tail id, not the latency one.
///
/// The two are separate properties with separate consequences — latency
/// re-plans PDC, tail resizes a bounce — so a host that mapped both onto one
/// signal would re-plan the graph every time a reverb's decay moved.
#[test]
fn a_tail_change_reaches_a_watching_host_under_its_own_id() {
    let w = Watched::new();

    let new_tail = PROBE_INITIAL_TAIL_SECONDS * 4.0;
    set_tail_seconds(&w.au, new_tail);

    assert!(
        w.log.wait_for(K_AUDIO_UNIT_PROPERTY_TAIL_TIME),
        "no TailTime PropertyChanged arrived within {SETTLE:?}; delivered {:?}",
        w.log.snapshot()
    );
    let delivered = w.log.snapshot();
    assert!(
        !delivered.contains(&K_AUDIO_UNIT_PROPERTY_LATENCY),
        "a tail change must not be reported as a latency change; got {delivered:?}"
    );

    let seconds =
        w.au.get_tail_time()
            .expect("the probe answers kAudioUnitProperty_TailTime");
    // `Seconds` is `f32` while the AU property is `Float64`, so the comparison
    // is in `f32` with a tolerance rather than exact: the narrowing is the
    // host's documented stopping point for the unit types, not an error here.
    assert!(
        (seconds.get() - new_tail as f32).abs() < 1e-6,
        "expected the new tail {new_tail}, got {seconds:?}"
    );
}

/// A parameter-list change must arrive, and the list the host then reads must
/// have actually moved.
///
/// The notification alone is not the property worth pinning: a host re-pulls the
/// list in response, and if the AU still reports the old one the re-pull is
/// wasted work that looks like it succeeded. The count assertion is what makes
/// the notification mean something.
#[test]
fn a_parameter_list_change_reaches_a_watching_host() {
    let w = Watched::new();

    let before = w.au.parameters().list();
    assert_eq!(
        before.len(),
        PROBE_INITIAL_PARAM_COUNT as usize,
        "the probe must declare a known list before the change, or 'the list \
         grew' has no baseline"
    );

    let grown = PROBE_INITIAL_PARAM_COUNT + 3;
    set_param_count(&w.au, grown);

    assert!(
        w.log.wait_for(K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST),
        "no ParameterList PropertyChanged arrived within {SETTLE:?}; delivered {:?}",
        w.log.snapshot()
    );

    let after = w.au.parameters().list();
    assert_eq!(
        after.len(),
        grown as usize,
        "the AU must already report the grown list by the time it notifies"
    );
    // And the new entries carry their own declared bounds, which is what a host
    // re-pulling the list is actually after — a list that grew by ids alone
    // would leave every new parameter unwritable through a range table.
    for p in &after {
        let (min, max) = support::probe_au::probe_param_bounds(p.id);
        assert_eq!((p.range.min, p.range.max), (min, max), "param {}", p.id);
    }
}

/// A watcher on these three must not be woken by an unrelated property.
///
/// The assertion that makes the three above mean something: AudioToolbox accepts
/// every property id at registration without validating it, so a `watch_property`
/// that registered the wrong *event type* — or subscribed to everything — would
/// still pass a delivery test. A host that re-planned PDC on every bypass toggle
/// would rebuild its graph continuously.
///
/// The positive control runs after the silence check, for the reason
/// `au_api_surface.rs`'s twin gives: without it, a machine where notifications
/// never work at all would pass the silence half trivially.
#[test]
fn a_watcher_is_not_woken_by_an_unrelated_property() {
    let mut w = Watched::new();

    w.au.set_bypass(true)
        .expect("the probe accepts a bypass write");
    w.au.set_bypass(false).expect("bypass off");

    std::thread::sleep(SETTLE);
    assert_eq!(
        w.log.snapshot(),
        Vec::<u32>::new(),
        "a latency/tail/parameter-list watcher must not see BypassEffect changes"
    );

    // Control: one of the three watched properties does arrive on the same
    // listener, so the silence above is filtering rather than a dead path.
    set_latency_seconds(&w.au, 128.0 / RATE);
    assert!(
        w.log.wait_for(K_AUDIO_UNIT_PROPERTY_LATENCY),
        "the control half failed: even a watched Latency change did not arrive, \
         so the silence above proves nothing about filtering"
    );
}

/// Dropping the listener must stop delivery.
///
/// `AUListenerDispose` is what keeps AudioToolbox from calling into a freed
/// closure, and the host installs one of these per plugin instance for the
/// instance's whole life. A registration that outlived its `Drop` would be a
/// use-after-free on the next property change the plugin made — at any time,
/// from its own editor.
#[test]
fn a_dropped_listener_stops_receiving() {
    let mut w = Watched::new();

    // Prove the path is live first, so "nothing arrived after the drop" is not
    // the same observation as "nothing ever arrived".
    set_latency_seconds(&w.au, 64.0 / RATE);
    assert!(
        w.log.wait_for(K_AUDIO_UNIT_PROPERTY_LATENCY),
        "the pre-drop control failed; nothing below would be meaningful"
    );

    w.drop_listener();
    // Settle before sampling the baseline: an event already in flight when
    // `AUListenerDispose` ran may still land, and counting it as a post-drop
    // delivery would make this flaky rather than strict.
    std::thread::sleep(SETTLE);
    let before = w.log.count.load(Ordering::Acquire);

    for frames in [128.0, 256.0, 512.0] {
        set_latency_seconds(&w.au, frames / RATE);
    }
    std::thread::sleep(SETTLE);

    assert_eq!(
        w.log.count.load(Ordering::Acquire),
        before,
        "the listener delivered after being dropped; delivered {:?}",
        w.log.snapshot()
    );
}
