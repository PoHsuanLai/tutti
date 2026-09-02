//! The public API this crate ships but no other suite calls.
//!
//! Every other suite here is organised around a *capability* — transport, MIDI
//! output, offline render — and covers whatever functions that capability needs.
//! This one is organised around the gap that leaves: symbols the crate exports,
//! and that a host would reach for, which no test mentions. An exported function
//! nothing calls is not merely unverified, it is unverified *and* published, so a
//! regression in it reaches a consumer before it reaches a test.
//!
//! ## What is covered, and why each was a real hole
//!
//! * **The `_at` display family** — [`parameters::value_strings_at`],
//!   [`parameters::string_from_value_at`], [`parameters::value_from_string_at`],
//!   [`parameters::clump_name_at`]. `au_multibus.rs` proves `list_at` / `get_at` /
//!   `set_at` address a *different element*; nothing proved the same for the
//!   display metadata, and the display family addresses by **scope** rather than
//!   by element (the parameter id occupies the element slot for
//!   `value_strings_at`, see `parameters::info_at`). So the un-suffixed wrappers
//!   passing `GLOBAL` are the whole contract, and it was untested: a wrapper that
//!   passed the input scope instead would return nothing for every parameter in
//!   the crate, silently.
//!
//! * **[`AuParameterListener::watch_property`]** — the one `watch_*` method with
//!   no delivery test. Registration returning `noErr` proves nothing here (see
//!   [`registration_never_refuses_a_property_id`]: AudioToolbox accepts
//!   `u32::MAX`), so the tests below drive a host-triggered property change
//!   end-to-end. That the host *can* trigger one was not obvious and is the
//!   finding that made this testable at all — see the module section below.
//!
//! * **[`TransportState::is_recording`] / [`TransportState::is_cycling`]** —
//!   `au_transport.rs` drives the underlying atomics through the `extern "C"`
//!   procs, which is the code AudioToolbox actually calls, but it never calls
//!   these two accessors. An accessor reading its *neighbouring* atomic passes
//!   every existing test in the crate.
//!
//! * **[`MidiOutputInfo::stream_count`]**, [`component::find_component`]'s
//!   negative and wildcard cases, and [`types::fourcc_to_string`]'s non-UTF-8
//!   path.
//!
//! ## `watch_property` CAN be driven from the host side
//!
//! The module docs for [`listener`] frame property changes as coming from the
//! plugin's own UI, which a test cannot drive. Measured on macOS 15.6, that is
//! only half true: AudioToolbox posts `kAudioUnitEvent_PropertyChange` for
//! property writes the **host** makes through `AudioUnitSetProperty` as well.
//! Verified over 10 trials per case, every trial identical:
//!
//! | host call | property posted | first event at |
//! |---|---|---|
//! | `AuInstance::set_bypass` | `kAudioUnitProperty_BypassEffect` (21) | 202–216 ms |
//! | `AuInstance::load_factory_preset` | `kAudioUnitProperty_PresentPreset` (36) | 204–214 ms |
//!
//! Those latencies track [`AuParameterListener`]'s 200 ms notification interval,
//! so [`SETTLE`] is set well above them.
//!
//! Two host calls that do **not** post an event, also measured: `get_state` +
//! `set_state` (no `kAudioUnitProperty_ClassInfo` event in 1.5 s), and
//! `set_render_quality`. Neither is asserted as a *negative* below, because
//! "AudioToolbox chose not to post" is not a contract of this crate.
//!
//! ### What remains unexercised, stated plainly
//!
//! * **A property change originating in the plugin's own editor.** Still
//!   undrivable: it needs a mouse in a Cocoa view. The decode path is shared with
//!   the host-triggered case — the same
//!   `AuEvent::from_raw` arm builds `PropertyChanged` for both — so the *decode*
//!   is covered; what is not covered is that an AU editor emits at all, which is
//!   the AU's behaviour rather than this crate's.
//! * **A non-global registration scope.** [`watch_property`] takes an
//!   [`EventAddress`], and registering on the input scope was measured to still
//!   deliver a **global-scope** event (`scope: 0`) for a global-scope property
//!   write. So the address argument is passed through to AudioToolbox but does
//!   not filter delivery on the one property this can be driven with. Recorded
//!   here rather than asserted: it is AudioToolbox's dispatch rule, not ours.
//! * **The `Drop` ordering invariant itself.** [`a_dropped_property_listener_stops_delivering`]
//!   proves no callback arrives after teardown, which is what a use-after-free
//!   would violate — but a passing run is not proof of absence of UB, only of
//!   absence of *delivery*. `au_notification.rs::a_dropped_listener_stops_delivering`
//!   makes the same trade for parameters.
//!
//! ## Each test was proven load-bearing by mutating the source
//!
//! A test that cannot fail is worse than none, so every assertion below was
//! checked against a deliberate break in `src/`, reverted after:
//!
//! | mutation | caught by |
//! |---|---|
//! | `value_strings` passes the input scope instead of `GLOBAL` | `value_strings_at_is_answered_only_on_the_global_scope` |
//! | `clump_name` passes the input scope instead of `GLOBAL` | `clump_name_at_is_answered_only_on_the_global_scope` |
//! | `TransportState::is_recording` reads the `cycling` atomic | `is_recording_and_is_cycling_read_their_own_flags` |
//! | `stream_count` counts only the `Some` entries | `stream_count_is_the_length_of_the_name_list_including_unnamed_slots` |
//! | `fourcc_to_string` decodes little-endian | `fourcc_to_string_is_big_endian_and_survives_non_utf8` |
//! | `find_component` drops its null check | `find_component_reports_absence_rather_than_a_null_handle` |
//! | `watch_property` registers `ParameterValueChange` instead of `PropertyChange` | all five `watch_property` tests |
//!
//! That last row is worth its own note, because it is what makes registration
//! failure *informative*: AudioToolbox validates **parameter** ids but not
//! **property** ids, so the tag confusion surfaces as
//! `kAudioUnitErr_InvalidParameter` (-10878) at registration for the very ids
//! [`registration_never_refuses_a_property_id`] asserts are accepted.
//!
//! One mutation was **not** caught, and the test that should have caught it says
//! so: see [`the_at_string_conversions_report_absence_rather_than_fabricating_zero`].
//!
//! Every row was re-checked after the coalescing fix described in
//! [`EventLog::settle_then_clear`] loosened three exact-sequence assertions into
//! set membership, because loosening an assertion is exactly how a test stops
//! being load-bearing. All five `watch_property` tests still fail under the tag
//! mutation.
//!
//! ## Judged internal rather than tested
//!
//! `RenderScratch::bind_output` (`src/buffer.rs`) and `CfString`/`CfUrl`/
//! `CfArray`/`CfPlist`'s `as_raw` / `value_at` / `to_binary` / `from_binary`
//! (`src/cf.rs`) read as public in their `impl` blocks but are **not** crate
//! public API: `mod buffer` and `mod cf` are private in `lib.rs`, and the types
//! are `pub(crate)`. There is nothing for an integration test to reach, which is
//! why they are absent below and why no `pub` needs reducing — the module
//! privacy already does it. `buffer_list_bytes` / `bind` / `from_binary` all
//! carry unit tests in their own modules.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_api_surface
//! ```

#![cfg(target_os = "macos")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod support;
use support::corpus::{DELAY, DISTORTION, MULTI_CHANNEL_MIXER, N_BAND_EQ};

use tutti_au_host::component::{self, AuType};
use tutti_au_host::midi_out::MidiOutputInfo;
use tutti_au_host::parameters::{self, ParamAddress};
use tutti_au_host::types::{
    self, AudioComponentDescription, K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT,
    K_AUDIO_UNIT_PROPERTY_LATENCY, K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET, K_AUDIO_UNIT_SCOPE_GLOBAL,
    K_AUDIO_UNIT_TYPE_EFFECT,
};
use tutti_au_host::{AuEvent, AuParameterListener, EventAddress};
use tutti_au_host::{BusDirection, TransportInfo, TransportState};

/// Serializes AudioToolbox discovery / instantiate / dispose, exactly as the
/// `AU_LOCK` of the same name in `au_conformance.rs` and its siblings does:
/// AudioToolbox tolerates concurrent use of *distinct* units, but discovery walks
/// a process-global registry and these tests open the same units the other suites
/// do.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// Poison recovery. The guard is a serializer only — it protects no shared
/// state — so one panicking test must not convert into N spurious failures.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// How long to wait for a property notification before calling it absent.
///
/// The listener's notification interval is 200 ms and the first event was
/// measured arriving at 202–216 ms across 25 trials, so this is ~14x the observed
/// latency. Generous deliberately, for the reason `au_notification.rs::SETTLE`
/// documents: too generous costs a slow *passing* test, too tight costs a flaky
/// failure on a loaded machine.
const SETTLE: Duration = Duration::from_secs(3);

/// How long to wait before concluding no *further* event will arrive.
///
/// Must comfortably exceed one full 200 ms notification interval so an in-flight
/// event has time to land and be counted — otherwise "nothing arrived" would only
/// mean "we did not wait long enough". Matches `au_notification.rs::QUIESCE`.
const QUIESCE: Duration = Duration::from_millis(900);

// -------------------------------------------------------------- the event log

/// Collects events off the private dispatch queue the listener delivers on.
///
/// A `Vec` behind a `Mutex` rather than a channel, for the reason
/// `au_notification.rs`'s log of the same shape gives: the assertions inspect the
/// whole ordered history repeatedly rather than consuming it once.
#[derive(Clone, Default)]
struct EventLog(Arc<Mutex<Vec<AuEvent>>>);

impl EventLog {
    fn sink(&self) -> impl Fn(AuEvent) + Send + Sync + 'static {
        let inner = Arc::clone(&self.0);
        move |ev| inner.lock().unwrap_or_else(|e| e.into_inner()).push(ev)
    }

    fn snapshot(&self) -> Vec<AuEvent> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Block until at least `n` events have arrived, or `SETTLE` elapses.
    fn wait_for_at_least(&self, n: usize) -> bool {
        let start = Instant::now();
        while start.elapsed() < SETTLE {
            if self.len() >= n {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.len() >= n
    }

    /// Wait for arrivals to stop, then empty the log — a phase boundary a later
    /// assertion can count from.
    ///
    /// A bare [`Self::clear`] is **not** a phase boundary, and assuming it was
    /// cost a flake that failed ~1 run in 5 on a loaded machine. The listener
    /// coalesces over a 200 ms interval, so [`Self::wait_for_at_least`] returning
    /// on the first event says nothing about the rest of that interval; whatever
    /// is still in flight lands after the `clear` and is then counted against the
    /// *next* phase's writes. Draining to a fixed point first is what makes the
    /// count after this call attributable.
    fn settle_then_clear(&self) {
        let mut settled = self.len();
        loop {
            std::thread::sleep(QUIESCE);
            let now = self.len();
            if now == settled {
                break;
            }
            settled = now;
        }
        self.clear();
    }
}

/// Every `PropertyChanged` id in `events`, in arrival order. Panics on any other
/// variant: a `ParameterChanged` reaching a property-only listener would mean the
/// event tag was decoded wrong, and silently filtering it out is how that hides.
fn property_ids(events: &[AuEvent]) -> Vec<u32> {
    events
        .iter()
        .map(|ev| match ev {
            AuEvent::PropertyChanged { id, .. } => *id,
            other => panic!(
                "a listener watching only properties received {other:?} — the \
                 event tag was decoded as the wrong kind"
            ),
        })
        .collect()
}

// ------------------------------------------------------- watch_property

/// A host-made property write must reach a listener registered for that property.
///
/// Without this the whole `watch_property` path is dead code that returns
/// `noErr`: a DAW that mirrors an AU's bypass state into its own channel strip
/// would show the plugin as active while the AU passes audio through untouched,
/// and would never learn otherwise because nothing tells it.
///
/// The *id* is asserted, not merely the arrival, because `AudioUnitEvent` is a C
/// union — `mParameterID` and `mPropertyID` occupy the same offset — and the
/// event tag is the only thing that says which name applies. A decode that read
/// the wrong union member would still deliver an event, carrying garbage.
///
/// Measured on macOS 15.6 over 10 trials, identical every trial: `set_bypass`
/// posts exactly one `kAudioUnitProperty_BypassEffect` (21) event, first arrival
/// 202–216 ms.
#[test]
fn a_host_property_write_reaches_a_property_listener() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let log = EventLog::default();
    // SAFETY: `au` outlives `listener` — `listener` is dropped at the end of this
    // scope, before `au`, which is `AuParameterListener::new`'s requirement.
    let listener = unsafe { AuParameterListener::new(au.raw_unit(), log.sink()) }
        .expect("create a listener on AUDelay");
    listener
        .watch_property(K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT, EventAddress::GLOBAL)
        .expect("watch_property on kAudioUnitProperty_BypassEffect");

    au.set_bypass(true).expect("AUDelay accepts a bypass write");

    assert!(
        log.wait_for_at_least(1),
        "no PropertyChanged arrived within {SETTLE:?} — measured first arrival is \
         202-216 ms, so this is a dead notification path, not a slow one"
    );
    let ids = property_ids(&log.snapshot());
    assert!(
        ids.contains(&K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT),
        "expected kAudioUnitProperty_BypassEffect \
         ({K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT}) among {ids:?}"
    );
}

/// A listener must receive *only* the properties it subscribed to.
///
/// This is the assertion that makes the one above mean something. AudioToolbox
/// accepts every property id at registration without validating it (see
/// [`registration_never_refuses_a_property_id`]), so a `watch_property` that
/// registered the wrong event *type* — or that subscribed to everything — would
/// still pass a delivery test. Here the listener watches `Latency` and the host
/// changes `BypassEffect`: silence is the only correct answer.
///
/// The consequence if this regressed: a host that watches `Latency` to know when
/// to re-run plugin delay compensation would re-run it on every bypass toggle and
/// every preset change, rebuilding its graph continuously.
///
/// Measured over 5 trials: 0 events in 1.2 s, every trial.
#[test]
fn a_property_listener_does_not_receive_properties_it_did_not_watch() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let log = EventLog::default();
    // SAFETY: as above — the listener is dropped first.
    let listener =
        unsafe { AuParameterListener::new(au.raw_unit(), log.sink()) }.expect("create a listener");
    listener
        .watch_property(K_AUDIO_UNIT_PROPERTY_LATENCY, EventAddress::GLOBAL)
        .expect("watch_property on kAudioUnitProperty_Latency");

    au.set_bypass(true).expect("bypass on");
    au.set_bypass(false).expect("bypass off");

    // Positive control first: the *same* write, watched, does arrive. Without it
    // this test would pass on a machine where notifications never work at all.
    std::thread::sleep(QUIESCE);
    assert_eq!(
        log.snapshot(),
        Vec::new(),
        "a Latency watcher must not see BypassEffect changes"
    );

    listener
        .watch_property(K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT, EventAddress::GLOBAL)
        .expect("now also watch BypassEffect");
    au.set_bypass(true).expect("bypass on again");
    assert!(
        log.wait_for_at_least(1),
        "the control half failed: even a subscribed BypassEffect change did not \
         arrive, so the silence above proves nothing about filtering"
    );
    // Every id, not the count: the listener coalesces over a 200 ms interval, so
    // how many events one write produces is AudioToolbox's scheduling rather than
    // a contract. What *is* a contract is that no other property appears.
    let ids = property_ids(&log.snapshot());
    assert!(
        ids.iter()
            .all(|id| *id == K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT),
        "only the newly-watched property may appear; got {ids:?}"
    );
}

/// Two distinct properties, two distinct ids — the union decode must not collapse
/// them.
///
/// `AuEvent::PropertyChanged` carries the raw `kAudioUnitProperty_*` id because
/// the set is open, so the id is the *only* thing a host can dispatch on. A
/// decode that hard-coded an id, or that read a constant offset from the wrong
/// union member, would deliver two indistinguishable events and a host would
/// mirror bypass state onto its preset menu.
///
/// Measured on macOS 15.6, AUDistortion, 10 trials: `load_factory_preset(5)`
/// posts exactly one `PresentPreset` (36); `set_bypass` posts one
/// `BypassEffect` (21).
#[test]
fn two_watched_properties_arrive_under_their_own_ids() {
    let _g = lock();
    let mut au = DISTORTION.open(RATE, BLOCK);

    let log = EventLog::default();
    // SAFETY: as above.
    let listener = unsafe { AuParameterListener::new(au.raw_unit(), log.sink()) }
        .expect("create a listener on AUDistortion");
    listener
        .watch_property(K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET, EventAddress::GLOBAL)
        .expect("watch PresentPreset");
    listener
        .watch_property(K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT, EventAddress::GLOBAL)
        .expect("watch BypassEffect");

    // AUDistortion ships 22 factory presets, numbered 0..=21 (see
    // `corpus::PRESET_EFFECTS`), so 5 is a real one.
    au.load_factory_preset(5).expect("load factory preset 5");
    assert!(
        log.wait_for_at_least(1),
        "no event for the preset change within {SETTLE:?}"
    );
    // The *set* of ids, not the count: coalescing over the 200 ms interval makes
    // the number of events AudioToolbox's scheduling rather than a contract. Which
    // id appears is the contract, and it is what a collapsed union decode breaks.
    let preset_phase = property_ids(&log.snapshot());
    assert!(
        preset_phase
            .iter()
            .all(|id| *id == K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET),
        "a preset load posts PresentPreset and nothing else; got {preset_phase:?}"
    );

    // A drain, not a bare clear: the preset phase's own coalesced tail would
    // otherwise land in the bypass phase and read as the two ids collapsing —
    // which is the very bug this test exists to catch, so the boundary has to be
    // real. See `EventLog::settle_then_clear`.
    log.settle_then_clear();

    au.set_bypass(true).expect("bypass on");
    assert!(
        log.wait_for_at_least(1),
        "no event for the bypass change within {SETTLE:?}"
    );
    let bypass_phase = property_ids(&log.snapshot());
    assert!(
        bypass_phase
            .iter()
            .all(|id| *id == K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT),
        "a bypass write posts BypassEffect, not PresentPreset — the two ids must \
         not collapse; got {bypass_phase:?}"
    );
}

/// Registration must not be mistaken for a capability check.
///
/// `watch_property` returns the AU's own status, and AudioToolbox was measured to
/// accept **every** property id including `u32::MAX` and ids no AU declares. So a
/// host cannot use a successful registration to decide whether a property exists,
/// and — the reason this is a test rather than a doc note — a delivery test that
/// only checked `Ok(())` would be vacuous.
///
/// Pinned so that if a future macOS *does* start validating, the change surfaces
/// here rather than as a host that silently stops watching.
#[test]
fn registration_never_refuses_a_property_id() {
    let _g = lock();
    let au = DELAY.open(RATE, BLOCK);

    let log = EventLog::default();
    // SAFETY: as above.
    let listener =
        unsafe { AuParameterListener::new(au.raw_unit(), log.sink()) }.expect("create a listener");

    for id in [0u32, 999_999, u32::MAX] {
        assert!(
            listener.watch_property(id, EventAddress::GLOBAL).is_ok(),
            "AudioToolbox was measured to accept property id {id}; a refusal here \
             is a real behaviour change, not a bug in this test"
        );
    }

    // And accepting the registrations does not conjure events out of nothing.
    std::thread::sleep(QUIESCE);
    assert_eq!(
        log.snapshot(),
        Vec::new(),
        "registering for undeclared properties must not deliver anything"
    );
}

/// Dropping the listener must stop delivery before the callback closure is freed.
///
/// `AuParameterListener::drop` calls `AUListenerDispose` *before* releasing the
/// dispatch queue and the block, and the ordering is the whole reason the type
/// exists. Freeing the block first leaves AudioToolbox holding a live
/// registration that points at a freed closure, and the next property change on
/// that AU — which the host itself can cause, as the tests above show — calls it.
///
/// 40 post-teardown property writes were measured to deliver 0 events across 5
/// trials. A passing run is evidence of no *delivery*, not a proof of no UB; the
/// same trade `au_notification.rs::a_dropped_listener_stops_delivering` makes.
///
/// ## Why the pre-teardown traffic is drained before the drop
///
/// The obvious shape — write, wait for one event, drop, clear, write again — is
/// **wrong**, and it cost a flake to find: it failed roughly 1 run in 5 on a
/// loaded machine, reporting exactly 40 late events. The cause is coalescing, not
/// a stale registration. `AuParameterListener` runs at a 200 ms notification
/// interval, so the *first* event for the pre-teardown write can be followed by
/// more from the same interval; `wait_for_at_least(1)` returns as soon as one
/// lands, and anything still in flight arrives after `clear()` and is then
/// misattributed to the post-teardown writes.
///
/// So the listener is quiesced — a full `QUIESCE` with no new arrivals — *before*
/// it is dropped. That is what makes the count afterwards attributable to the
/// writes that follow the dispose, which is the only thing a stale registration
/// could explain.
#[test]
fn a_dropped_property_listener_stops_delivering() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    let log = EventLog::default();
    // SAFETY: as above.
    let listener =
        unsafe { AuParameterListener::new(au.raw_unit(), log.sink()) }.expect("create a listener");
    listener
        .watch_property(K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT, EventAddress::GLOBAL)
        .expect("watch BypassEffect");

    au.set_bypass(true).expect("bypass on");
    assert!(
        log.wait_for_at_least(1),
        "the listener must be proven live before its teardown is tested — \
         otherwise the silence afterwards means nothing"
    );

    // Drain the notification interval the write above opened. Without this, its
    // own coalesced tail lands after the clear and is counted against the
    // post-teardown writes — see this test's docs.
    log.settle_then_clear();

    drop(listener);

    // Enough writes that a stale registration would almost certainly be hit.
    for i in 0..40 {
        au.set_bypass(i % 2 == 0)
            .expect("bypass write after teardown");
    }
    std::thread::sleep(QUIESCE);
    assert_eq!(
        log.snapshot(),
        Vec::new(),
        "a disposed listener must receive nothing; anything here means the \
         registration outlived the closure"
    );
}

// ------------------------------------------------- the `_at` display family

/// `value_strings_at` must address by the scope it is given, and the un-suffixed
/// wrapper must therefore be passing `GLOBAL`.
///
/// This is the contract `au_multibus.rs` proves for `list_at`/`get_at`/`set_at`
/// and nobody proved for the display metadata. It matters because the display
/// family's addressing is **scope**-shaped rather than element-shaped: the
/// parameter id occupies the element slot (see `parameters::info_at`), so the
/// scope is the only part of the address that discriminates — and a wrapper that
/// passed the input scope would return an empty vec for every parameter in the
/// crate, which is also the legitimate "no value strings" answer. The bug would be
/// invisible: every enum parameter in every AU would silently render as bare
/// floats.
///
/// Measured on macOS 15.6, AUNBandEQ parameter 2000 ("Type"): 11 labels at the
/// global scope, **0** at the input scope and 0 at the output scope. That
/// asymmetry is what makes the assertion load-bearing.
#[test]
fn value_strings_at_is_answered_only_on_the_global_scope() {
    let _g = lock();
    let au = N_BAND_EQ.open(RATE, BLOCK);
    let unit = au.raw_unit();

    let global = parameters::value_strings_at(unit, ParamAddress::GLOBAL, 2000);
    assert_eq!(
        global.len(),
        11,
        "AUNBandEQ param 2000 publishes 11 filter names on the global scope; got \
         {global:?}"
    );
    assert_eq!(global.first().map(String::as_str), Some("Parametric"));

    for (label, addr) in [
        ("input", ParamAddress::on_bus(BusDirection::Input, 0)),
        ("output", ParamAddress::on_bus(BusDirection::Output, 0)),
    ] {
        let scoped = parameters::value_strings_at(unit, addr, 2000);
        assert!(
            scoped.is_empty(),
            "AUNBandEQ answers value strings on the global scope only; the \
             {label} scope returned {scoped:?}. If this now has content, the \
             asymmetry the un-suffixed wrapper relies on is gone."
        );
    }

    // And the un-suffixed wrapper agrees with the global address, which is the
    // property that would break if it started passing a bus scope.
    assert_eq!(
        parameters::value_strings(unit, 2000),
        global,
        "`value_strings` must be `value_strings_at(GLOBAL)`"
    );
}

/// `clump_name_at` must address by the scope it is given, and `clump_name` must
/// therefore be passing `GLOBAL`.
///
/// The same shape as the value-strings case above, on the other display property,
/// and with the same invisible failure: a wrapper querying a bus scope returns
/// `None` for every clump, which is indistinguishable from an AU that groups
/// nothing. A 41-parameter EQ or a 400-parameter synth would render as one flat
/// alphabetical list and no test would notice.
///
/// Measured on macOS 15.6, AUDistortion: clumps 1..=7 all named on the global
/// scope (`Delay`, `Ring Modulation`, `Decimation`, `Cubic Polynomial`,
/// `Soft Clip`, `Filter`, `Mix`), and **none** named on the input or output scope.
#[test]
fn clump_name_at_is_answered_only_on_the_global_scope() {
    let _g = lock();
    let au = DISTORTION.open(RATE, BLOCK);
    let unit = au.raw_unit();

    // The full measured table, so a shifted or truncated read is caught rather
    // than merely a read that returns nothing.
    let expected = [
        (1u32, "Delay"),
        (2, "Ring Modulation"),
        (3, "Decimation"),
        (4, "Cubic Polynomial"),
        (5, "Soft Clip"),
        (6, "Filter"),
        (7, "Mix"),
    ];
    for (clump, name) in expected {
        assert_eq!(
            parameters::clump_name_at(unit, ParamAddress::GLOBAL, clump).as_deref(),
            Some(name),
            "AUDistortion clump {clump} is named {name:?} on the global scope"
        );
    }
    // Clump 8 does not exist — the read must stop rather than run off the table.
    assert_eq!(
        parameters::clump_name_at(unit, ParamAddress::GLOBAL, 8),
        None,
        "AUDistortion names 7 clumps; an 8th name means the read is fabricating"
    );

    for (label, addr) in [
        ("input", ParamAddress::on_bus(BusDirection::Input, 0)),
        ("output", ParamAddress::on_bus(BusDirection::Output, 0)),
    ] {
        for (clump, _) in expected {
            assert_eq!(
                parameters::clump_name_at(unit, addr, clump),
                None,
                "AUDistortion names clumps on the global scope only; the {label} \
                 scope named clump {clump}"
            );
        }
    }

    // The un-suffixed wrapper must be the global address.
    for (clump, name) in expected {
        assert_eq!(
            parameters::clump_name(unit, clump).as_deref(),
            Some(name),
            "`clump_name` must be `clump_name_at(GLOBAL)`"
        );
    }
}

/// The string↔value conversions report absence at *every* address, and the
/// `_at` forms agree with their wrappers.
///
/// `au_param_display.rs` pins the measured absence at the global scope: no Apple
/// AU on macOS 15.6 implements `ParameterStringFromValue` or
/// `ParameterValueFromString`. This extends that to the per-element forms, which
/// are what a mixer strip UI would call, and asserts the honest `None` rather
/// than a fabricated success — relaxing it to "either works or doesn't" would
/// make it unfalsifiable.
///
/// ## What this test does NOT cover, verified by mutation
///
/// `value_from_string_at` seeds `outValue` with `f32::NAN` and rejects a
/// non-finite result, so that an AU returning `noErr` without writing the field
/// cannot be read as a genuine parse of `0.0` — a value the host would then
/// commit to the user's preset. **This test cannot catch a regression in that
/// sentinel.** Replacing `f32::NAN` with `0.0` was tried against this suite and
/// every assertion still passed, because the guard is
/// `status != NO_ERR || !outValue.is_finite()` and on Apple's units the *status*
/// arm always fires first — the sentinel is never reached. Proving it needs an AU
/// that answers the property, and none exists on this machine (see the module
/// docs' measured-absence table). Recorded here rather than left as an implied
/// claim: the assertions below pin the `None`, not the mechanism that produces it.
///
/// Subject: AUMultiChannelMixer, whose 8 input elements each carry a 7-parameter
/// strip (ids 0 `Gain`, 1 `Enable`, 2 `Pan`, plus 4 metering pseudo-parameters),
/// measured on macOS 15.6. Every `(scope, element, id, value)` combination below
/// returned `None`.
#[test]
fn the_at_string_conversions_report_absence_rather_than_fabricating_zero() {
    let _g = lock();
    // Hosted uninitialized: AUMultiChannelMixer's per-element parameters are
    // readable in the Loaded state, and `au_multibus.rs` documents why the mixers
    // are hosted this way (they refuse `AudioUnitInitialize` as this host
    // configures them).
    let au = MULTI_CHANNEL_MIXER.open_uninitialized(RATE, BLOCK);
    let unit = au.raw_unit();

    let addresses = [
        ("global", ParamAddress::GLOBAL),
        (
            "input element 0",
            ParamAddress::on_bus(BusDirection::Input, 0),
        ),
        (
            "input element 1",
            ParamAddress::on_bus(BusDirection::Input, 1),
        ),
        (
            "input element 7",
            ParamAddress::on_bus(BusDirection::Input, 7),
        ),
        (
            "output element 0",
            ParamAddress::on_bus(BusDirection::Output, 0),
        ),
    ];
    // Gain, Enable and Pan — the three real controls on the strip.
    for id in [0u32, 1, 2] {
        for (label, addr) in addresses {
            for value in [0.0f32, 0.5, 1.0] {
                assert_eq!(
                    parameters::string_from_value_at(unit, addr, id, value),
                    None,
                    "AUMultiChannelMixer does not implement \
                     ParameterStringFromValue; {label} id {id} value {value} \
                     answered something"
                );
            }
            for text in ["0.5", "-6 dB", "Off"] {
                assert_eq!(
                    parameters::value_from_string_at(unit, addr, id, text),
                    None,
                    "AUMultiChannelMixer does not implement \
                     ParameterValueFromString; {label} id {id} text {text:?} \
                     answered something. If it is `Some`, re-measure before \
                     relaxing this — a value fabricated from a failed parse is \
                     what gets committed to the user's preset."
                );
            }
        }
    }

    // The wrappers must delegate to the global address, not to some other one.
    assert_eq!(
        parameters::string_from_value(unit, 0, 0.5),
        parameters::string_from_value_at(unit, ParamAddress::GLOBAL, 0, 0.5)
    );
    assert_eq!(
        parameters::value_from_string(unit, 0, "0.5"),
        parameters::value_from_string_at(unit, ParamAddress::GLOBAL, 0, "0.5")
    );
}

/// A per-element read must not be answered from element 0's parameter.
///
/// `value_at`-shaped bugs aside, the risk specific to the display family is the
/// opposite of `au_multibus.rs`'s: those functions put the **parameter id** in the
/// element slot, so a maintainer "fixing" them to pass `addr.element` instead
/// would query metadata for whatever parameter happened to share that number.
/// AUMultiChannelMixer is the subject because its ids (0, 1, 2, 1000, 2000, 3000,
/// 4000) overlap its element indices (0..=7) — so element 1 and parameter id 1
/// are both live, and confusing them is representable.
///
/// Measured on macOS 15.6: the input strip is identical on every element (ids
/// `[0, 1, 1000, 2000, 3000, 4000, 2]`, 7 parameters), and the output element
/// carries a different set of 5 — so a read that collapsed the scope would report
/// the wrong arity.
#[test]
fn the_mixer_strip_arity_differs_between_input_and_output_scope() {
    let _g = lock();
    let au = MULTI_CHANNEL_MIXER.open_uninitialized(RATE, BLOCK);
    let unit = au.raw_unit();

    // The global scope has no parameters at all on this unit, which is the fact
    // that makes an accidentally-global read detectable rather than plausible.
    assert!(
        parameters::list_at(unit, ParamAddress::GLOBAL).is_empty(),
        "AUMultiChannelMixer publishes no global-scope parameters"
    );

    for element in [0u32, 1, 7] {
        let strip = parameters::list_at(unit, ParamAddress::on_bus(BusDirection::Input, element));
        assert_eq!(
            strip.len(),
            7,
            "input element {element} carries a 7-parameter strip; got {:?}",
            strip.iter().map(|p| p.id).collect::<Vec<_>>()
        );
    }
    let out = parameters::list_at(unit, ParamAddress::on_bus(BusDirection::Output, 0));
    assert_eq!(
        out.len(),
        5,
        "the output element carries 5 parameters, not the input strip's 7 — a \
         read that ignored the scope would report the same arity for both"
    );
}

// -------------------------------------------------- transport state accessors

/// `is_recording` and `is_cycling` must each read their own atomic.
///
/// `au_transport.rs` drives the underlying flags through the `extern "C"` procs,
/// which is the code AudioToolbox actually calls — but it never calls these two
/// accessors, so an accessor reading its *neighbouring* atomic passes every test
/// in the crate today. `recording` and `cycling` are declared adjacently in
/// `TransportState` and are both plain `AtomicBool`, so nothing but this
/// distinguishes them.
///
/// What a swap would cost a host: a DAW asking "is the transport cycling" to
/// decide whether to pre-roll a loop would answer from the record-arm state, and
/// a punch-in recorder would arm on loop enable.
///
/// The four combinations are all exercised, because two accessors reading one
/// atomic agree on the diagonal — asserting only `(true, true)` and
/// `(false, false)` would pass with the bug.
#[test]
fn is_recording_and_is_cycling_read_their_own_flags() {
    for (recording, cycling) in [(false, false), (true, false), (false, true), (true, true)] {
        let state = TransportState::new();
        let info = TransportInfo::new()
            .with_playing(true)
            .with_recording(recording)
            // `with_loop`'s first argument is the cycle-active flag; the beats are
            // a valid non-empty region so the state is not rejected on some other
            // ground.
            .with_loop(cycling, 4.0, 20.0);
        state.set_transport(&info, false);

        assert_eq!(
            state.is_recording(),
            recording,
            "is_recording must report the recording flag, not the cycling one \
             (recording={recording}, cycling={cycling})"
        );
        assert_eq!(
            state.is_cycling(),
            cycling,
            "is_cycling must report the cycling flag, not the recording one \
             (recording={recording}, cycling={cycling})"
        );
        // The third neighbour, as the control: `playing` is set throughout, so an
        // accessor that had collapsed onto it would report `true` everywhere.
        assert!(state.is_playing(), "playing was published as true");
    }
}

/// A fresh `TransportState` must be stopped, not recording and not cycling.
///
/// This is the state an AU sees between `install_host_callbacks` and the host's
/// first `set_transport`, and it is a real window: `AuInstance::install_host_callbacks`
/// hands the AU a pointer to exactly this. A default that reported `recording`
/// would make a plugin arm itself on load.
#[test]
fn a_fresh_transport_state_is_stopped() {
    let state = TransportState::new();
    assert!(!state.is_playing(), "a fresh transport is stopped");
    assert!(!state.is_recording(), "a fresh transport is not recording");
    assert!(!state.is_cycling(), "a fresh transport is not cycling");
    assert!(
        !state.state_changed_pending(),
        "a fresh transport has no locate pending — an AU that saw one would flush \
         its sequencer on the first block for no reason"
    );
}

// ---------------------------------------------------------- MIDI output info

/// `stream_count` must be the length of the published name list.
///
/// It is derived rather than stored — `MidiOutputInfo` deliberately keeps no
/// separate count, because Apple defines the array's length *as* the number of
/// outputs and a second field could disagree with its source. This pins the
/// derivation, including the two cases that a hard-coded answer would get wrong:
/// the empty list (an AU that implements the property and currently has no
/// streams, which is distinct from `None`) and a list containing `None` entries.
///
/// The `None` case is the one worth spelling out. An entry is `None` when the AU
/// published a value that is not a usable `CFString`; the slot is **kept** rather
/// than skipped, because dropping it would renumber every output after it and
/// those numbers are the `midi_out_num` the render callback is keyed by. So a
/// `stream_count` that counted only the `Some` entries would under-report, and a
/// host routing by index would send stream 3's notes to stream 2.
///
/// A unit test rather than a corpus test because **no AU on this machine
/// publishes the property** — measured: 0 of 138 components, at any scope,
/// initialized or not (see `au_midi_out.rs::no_installed_au_publishes_midi_output_info`).
/// A hand-built value is the only way to exercise the accessor at all, and it is
/// honest about that.
#[test]
fn stream_count_is_the_length_of_the_name_list_including_unnamed_slots() {
    assert_eq!(
        MidiOutputInfo { names: Vec::new() }.stream_count(),
        0,
        "an AU that implements the property with no streams reports 0"
    );
    assert_eq!(
        MidiOutputInfo {
            names: vec![Some("Out 1".into())]
        }
        .stream_count(),
        1
    );
    assert_eq!(
        MidiOutputInfo {
            names: vec![Some("Out 1".into()), None, Some("Out 3".into())]
        }
        .stream_count(),
        3,
        "an unnamed slot still occupies a stream index — counting only the named \
         ones would renumber every output after it"
    );
    assert_eq!(
        MidiOutputInfo {
            names: vec![None, None]
        }
        .stream_count(),
        2,
        "two unusable names are still two streams"
    );
}

// -------------------------------------------------------------- find_component

/// An exact component description that matches nothing must yield `None`.
///
/// `find_component` returns `Option<AudioComponent>` by turning a null
/// `AudioComponentFindNext` result into `None`. If that null check were dropped —
/// or inverted — the function would hand back a null `AudioComponent`, and
/// `AuInstance::new` would call `AudioComponentInstanceNew` on it. The corpus's
/// `AuRef::require` is built directly on this path, so a null slipping through
/// turns every "unit is missing" diagnostic into a crash inside AudioToolbox.
///
/// `zzzz`/`zzzz` was measured absent on macOS 15.6, which is unsurprising but is
/// the measurement the assertion rests on.
#[test]
fn find_component_reports_absence_rather_than_a_null_handle() {
    let _g = lock();
    let missing = AudioComponentDescription {
        componentType: K_AUDIO_UNIT_TYPE_EFFECT,
        componentSubType: u32::from_be_bytes(*b"zzzz"),
        componentManufacturer: u32::from_be_bytes(*b"zzzz"),
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    assert!(
        component::find_component(&missing).is_none(),
        "no AU is registered as aufx/zzzz/zzzz"
    );
}

/// An all-zero description is AudioToolbox's **wildcard**, not an empty query.
///
/// This is the trap the function's shape invites: `AudioComponentDescription`
/// derives `Default`, so `find_component(&Default::default())` reads like "find
/// nothing in particular" and in fact returns the first AU on the system. A
/// caller that built a description field-by-field and forgot one would silently
/// bind to an unrelated plugin — which is exactly why `support/corpus.rs` matches
/// on subtype **and** manufacturer, and why the corpus rule is codes rather than
/// display names.
///
/// Measured on macOS 15.6: the all-zero description matches (the system has ~138
/// components registered, so the wildcard always finds one).
#[test]
fn an_all_zero_description_is_a_wildcard_that_matches() {
    let _g = lock();
    assert!(
        component::find_component(&AudioComponentDescription::default()).is_some(),
        "an all-zero description is AudioToolbox's wildcard and matches the first \
         registered component; a caller must not treat Default as 'no query'"
    );

    // And a wildcard *type* with a real subtype still resolves the corpus unit,
    // which is what makes the wildcard semantics concrete rather than incidental.
    let by_subtype_only = AudioComponentDescription {
        componentType: 0,
        componentSubType: u32::from_be_bytes(*b"dely"),
        componentManufacturer: u32::from_be_bytes(*b"appl"),
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    assert!(
        component::find_component(&by_subtype_only).is_some(),
        "zero in the type field matches any type, so appl/dely still resolves"
    );
}

/// `find_component` must resolve the same handle the enumerator does.
///
/// Two independent paths into AudioToolbox's registry —
/// `AudioComponentFindNext(null, exact_desc)` and the enumeration loop
/// `enumerate_components_of_type` runs — must agree, because the corpus reaches
/// units through the second and `parameters.rs`'s own unit tests reach them
/// through the first. A disagreement would mean one of them is matching on
/// something other than the codes.
#[test]
fn find_component_agrees_with_the_enumerator() {
    let _g = lock();
    let desc = AudioComponentDescription {
        componentType: K_AUDIO_UNIT_TYPE_EFFECT,
        componentSubType: u32::from_be_bytes(*b"dely"),
        componentManufacturer: u32::from_be_bytes(*b"appl"),
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    let found = component::find_component(&desc).expect("AUDelay ships with macOS");

    let enumerated = component::enumerate_components_of_type(AuType::Effect)
        .into_iter()
        .find(|c| {
            c.sub_type == u32::from_be_bytes(*b"dely")
                && c.manufacturer_code == u32::from_be_bytes(*b"appl")
        })
        .expect("the enumerator finds AUDelay too");

    assert_eq!(
        found, enumerated.component,
        "the two lookup paths must resolve the same factory handle"
    );
}

// ------------------------------------------------------------ fourcc_to_string

/// A four-char code decodes to exactly four characters, big-endian, and never
/// panics on bytes that are not UTF-8.
///
/// Every code this crate decodes comes from an AU, and third-party
/// `componentManufacturer` codes are *not* required to be printable ASCII — they
/// are an arbitrary `u32`. `AuComponentInfo::manufacturer` is built from this
/// function on every enumeration, so a decode that panicked or truncated on a
/// non-ASCII code would take down the plugin scan for one badly-behaved plugin,
/// and `AuType::Unknown`'s `Display` would take a host's plugin list with it.
///
/// The byte-length arithmetic is the load-bearing part and is measured, not
/// reasoned: `String::from_utf8_lossy` replaces each invalid byte with U+FFFD,
/// which is **3 bytes**, so `0xFFFFFFFF` decodes to 4 chars / 12 bytes. A
/// `len() == 4` assertion (the obvious one) would be wrong.
#[test]
fn fourcc_to_string_is_big_endian_and_survives_non_utf8() {
    // Big-endian order, not little: a byte-swapped decode would render "aufx"
    // as "xfua" and every AU type string in a browser would be reversed.
    assert_eq!(
        types::fourcc_to_string(u32::from_be_bytes(*b"aufx")),
        "aufx"
    );
    assert_eq!(
        types::fourcc_to_string(u32::from_be_bytes(*b"dely")),
        "dely"
    );
    // Trailing space is significant — DLSMusicDevice's subtype is `dls ` and
    // trimming it would stop it matching.
    assert_eq!(
        types::fourcc_to_string(u32::from_be_bytes(*b"dls ")),
        "dls "
    );

    // Measured on macOS 15.6: an all-invalid code yields 4 replacement
    // characters, which is 12 bytes. The *character* count is 4 in every case;
    // the byte count is not, which is why the assertion is on `chars()`.
    for code in [0x0000_0000u32, 0xFFFF_FFFF, 0x8081_8283] {
        let s = types::fourcc_to_string(code);
        assert_eq!(
            s.chars().count(),
            4,
            "{code:#010x} must decode to exactly 4 characters, got {s:?}"
        );
    }
    assert_eq!(
        types::fourcc_to_string(0xFFFF_FFFF).len(),
        12,
        "four U+FFFD replacement characters are 12 bytes — a `len() == 4` check \
         would be asserting the wrong quantity"
    );
    // A NUL code is four NUL *characters*, not an empty string: it must not be
    // silently trimmed into one, or two distinct manufacturers would collide.
    assert_eq!(types::fourcc_to_string(0).chars().count(), 4);
    assert!(!types::fourcc_to_string(0).is_empty());

    // Round-trip against the enumerator: every manufacturer code on the system
    // decodes to a 4-character string, so no real plugin can break the scan.
    let _g = lock();
    let all = component::enumerate_components_of_type(AuType::Effect);
    assert!(!all.is_empty(), "the system has effect AUs registered");
    for c in &all {
        assert_eq!(
            c.manufacturer.chars().count(),
            4,
            "{}: manufacturer {:?} decoded to {} characters",
            c.name,
            c.manufacturer,
            c.manufacturer.chars().count()
        );
        assert_eq!(
            c.manufacturer,
            types::fourcc_to_string(c.manufacturer_code),
            "{}: the stored string must be the decode of the stored code",
            c.name
        );
    }
}
