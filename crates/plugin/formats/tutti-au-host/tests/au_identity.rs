//! Instance identity and UI-affordance properties against real AUs.
//!
//! Covers the four properties in `src/identity.rs`: `ContextName`, `NickName`,
//! `ParametersForOverview` and `IconLocation`. None of them change a sample —
//! they exist so a plugin's window and a host's browser can say something
//! truthful about *this* instance.
//!
//! ## What these tests are for
//!
//! A property that is merely *accepted* proves nothing: this crate has already
//! been bitten twice by that. `HostCallbacks` is accepted by ~35 units and
//! called by none, and `HostMIDIProtocol` stores the invalid value `99` and
//! reports it straight back. So every test here asserts a **round trip** — the
//! value the AU returns, not the status of the write — and the overview test
//! asserts the AU's answer differs from the naive one a host would otherwise
//! guess.
//!
//! ## Every test here was proven load-bearing by mutation
//!
//! Each mutation below was applied to `src/identity.rs`, the suite run, and the
//! mutation reverted.
//!
//! | mutation to `src/identity.rs` | caught by |
//! |---|---|
//! | `stride = 16` instead of `size_of::<AudioUnitParameter>()` (24) | `every_overview_entry_addresses_a_readable_parameter`, `the_overview_is_a_strict_subset_for_a_unit_that_curates` |
//! | `sort_unstable_by_key` the decoded overview | `the_overview_order_is_not_the_parameter_list_order` |
//! | `set_nick_name` writes `ContextName` | `a_nick_name_round_trips_and_can_be_replaced`, `a_nick_name_survives_non_ascii_text`, `two_instances_of_one_au_hold_independent_nick_names` |
//! | `icon_location` returns a path that does not exist | `an_icon_url_names_a_file_that_exists` |
//! | `parameters_for_overview` collapses `Err` into an empty list | `a_non_implementing_unit_reports_the_gap_rather_than_an_empty_list` |
//! | **`set_context_name` replaced by `Ok(())`** | `every_unit_round_trips_a_context_name` |
//! | **`nick_name` collapses `Err` into `Ok(None)`** | `the_string_readers_report_a_missing_property_as_an_error` |
//! | **`icon_location` collapses `Err` into `Ok(None)`** | `the_string_readers_report_a_missing_property_as_an_error` |
//! | **overview count `saturating_sub(1)`** (drop last entry) | `the_overview_decodes_every_entry_the_au_reported` |
//! | **`element: p.mElement.wrapping_add(7)`** | `the_overview_decodes_every_entry_the_au_reported` |
//!
//! The five in bold **survived the first version of this suite** and were found
//! by an adversarial review, not by the original audit. The first is the
//! instructive one: `set_context_name` could be replaced by `Ok(())` outright,
//! because the sweep asserted only that the write returned `Ok` — exactly the
//! "a property that is merely accepted proves nothing" trap this header warns
//! about, committed into the suite meant to prevent it. Choosing your own
//! mutations tests what you already thought of.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_identity
//! ```
//!
//! No SDK, no display, no env vars: the corpus ships with macOS, so a missing
//! unit is a hard failure rather than a skip. See `support/corpus.rs`.

#![cfg(target_os = "macos")]

mod support;

use std::sync::Mutex;

use support::corpus::{
    every_component, open_info, DELAY, DLS_SYNTH, DYNAMICS, INVALID_PROPERTY, MATRIX_REVERB,
};
use support::probe_au::{Misbehaviour, PROBE_MANUFACTURER};
use tutti_au_host::AuError;

/// AudioToolbox tolerates concurrent use of *distinct* units, but component
/// discovery walks a process-global registry, and the two sweeps here
/// instantiate every component on the machine. Running them concurrently
/// **segfaults** — reproduced by removing this guard, which turns a green run
/// into `signal: 11` while each test still passes in isolation. Mirrors
/// `AU_LOCK` in `au_conformance.rs`.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// `AU_LOCK` is poisoned by any panicking test, and a poisoned lock would
/// convert one real failure into N spurious ones. The guard is only a
/// serializer — there is no shared state to be left inconsistent — so recover.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every instantiable unit hands back the exact context name written.
///
/// Asserts the **read-back**, not the write status. An earlier version of this
/// test asserted only that the write returned `Ok`, which meant
/// `set_context_name` could be replaced by `Ok(())` and all nine tests still
/// passed — the precise failure this suite's header warns about, committed into
/// the suite meant to prevent it.
#[test]
fn every_unit_round_trips_a_context_name() {
    let _g = lock();
    let mut checked = 0;
    for info in every_component() {
        let Ok(au) = std::panic::catch_unwind(|| open_info(&info, 48_000.0, 512)) else {
            continue;
        };
        au.set_context_name("track 3")
            .unwrap_or_else(|e| panic!("{}: set_context_name failed: {e:?}", info.name));
        assert_eq!(
            au.context_name()
                .unwrap_or_else(|e| panic!("{}: context_name read failed: {e:?}", info.name)),
            Some("track 3".to_string()),
            "{}: the AU did not hand back the context name just written",
            info.name
        );
        checked += 1;
    }
    assert!(
        checked >= 40,
        "only {checked} units instantiated; the corpus should offer ~55, so \
         something is wrong with the harness rather than with the property"
    );
}

/// A second context name replaces the first.
///
/// Without this, a `context_name` that cached and echoed the first value it
/// ever saw would satisfy the sweep above.
#[test]
fn a_context_name_can_be_replaced() {
    let _g = lock();
    let au = DELAY.open(48_000.0, 512);

    au.set_context_name("track 3").expect("set_context_name");
    au.set_context_name("Drum Bus").expect("set_context_name");
    assert_eq!(
        au.context_name().expect("context_name"),
        Some("Drum Bus".to_string()),
        "the second write did not replace the first"
    );
}

/// A nickname survives the round trip through a real AU, and a *different*
/// nickname replaces it.
///
/// The second write is what makes this falsifiable. A `nick_name` that ignored
/// its argument and echoed a cached first value would pass a single-write test.
#[test]
fn a_nick_name_round_trips_and_can_be_replaced() {
    let _g = lock();
    let au = DELAY.open(48_000.0, 512);

    au.set_nick_name("Slapback").expect("set_nick_name");
    assert_eq!(
        au.nick_name().expect("nick_name"),
        Some("Slapback".to_string()),
        "the AU did not hand back the name just written"
    );

    au.set_nick_name("Long Tail").expect("set_nick_name");
    assert_eq!(
        au.nick_name().expect("nick_name"),
        Some("Long Tail".to_string()),
        "the second write did not replace the first — a cached echo would \
         report the original name here"
    );
}

/// Two instances of the *same* AU hold independent nicknames.
///
/// This is the property's entire reason to exist — telling two loads of one
/// plugin apart. A host-global or component-global store would fail here while
/// passing every single-instance test above.
#[test]
fn two_instances_of_one_au_hold_independent_nick_names() {
    let _g = lock();
    let a = DELAY.open(48_000.0, 512);
    let b = DELAY.open(48_000.0, 512);

    a.set_nick_name("Drums").expect("set_nick_name");
    b.set_nick_name("Vocals").expect("set_nick_name");

    assert_eq!(a.nick_name().expect("nick_name"), Some("Drums".to_string()));
    assert_eq!(
        b.nick_name().expect("nick_name"),
        Some("Vocals".to_string()),
        "the two instances share one name slot, so a session could not \
         distinguish them"
    );
}

/// Non-ASCII names survive intact.
///
/// The path crosses UTF-8 → CFString → UTF-8, and a host that names tracks in
/// any non-English language depends on it. A lossy conversion would mangle
/// these while leaving the ASCII tests green.
#[test]
fn a_nick_name_survives_non_ascii_text() {
    let _g = lock();
    let au = DELAY.open(48_000.0, 512);
    for name in ["ドラム", "Café — Bus 2", "трек 3", "🥁 kick"] {
        au.set_nick_name(name).expect("set_nick_name");
        assert_eq!(
            au.nick_name().expect("nick_name"),
            Some(name.to_string()),
            "{name:?} did not survive the CFString round trip"
        );
    }
}

/// The overview list is a genuine *subset* of the parameter list, not a copy.
///
/// If it merely echoed the parameter list, a host would gain nothing by asking.
/// AUMatrixReverb reports 8 of its 17 parameters (measured, macOS 15.6).
#[test]
fn the_overview_is_a_strict_subset_for_a_unit_that_curates() {
    let _g = lock();
    let au = MATRIX_REVERB.open(48_000.0, 512);

    let overview = au
        .parameters_for_overview()
        .expect("AUMatrixReverb implements ParametersForOverview");
    let all = au.parameters().list();

    assert!(
        !overview.is_empty(),
        "an implementing unit returned an empty overview"
    );
    assert!(
        overview.len() < all.len(),
        "overview has {} of {} parameters — not a curated subset, so reading \
         this property buys a host nothing over reading the parameter list",
        overview.len(),
        all.len()
    );

    let known: std::collections::HashSet<u32> = all.iter().map(|p| p.id).collect();
    for p in &overview {
        assert!(
            known.contains(&p.id),
            "overview names parameter {} which is not in the parameter list",
            p.id
        );
    }
}

/// The overview's order is the AU's priority order, not declaration order.
///
/// This is the claim that justifies the property. AUDynamicsProcessor leads
/// with ids `[4, 5, 6, 0, 1, 2]` — a host approximating the overview by taking
/// the first N parameters would show a different, worse set. A `sort()` slipped
/// into the decoder would be invisible without this.
#[test]
fn the_overview_order_is_not_the_parameter_list_order() {
    let _g = lock();
    let au = DYNAMICS.open(48_000.0, 512);

    let overview = au
        .parameters_for_overview()
        .expect("AUDynamicsProcessor implements ParametersForOverview");
    let ids: Vec<u32> = overview.iter().map(|p| p.id).collect();

    let mut ascending = ids.clone();
    ascending.sort_unstable();
    assert_ne!(
        ids, ascending,
        "the overview came back in ascending id order, so either the AU stopped \
         curating or the decoder sorted it — either way the priority the \
         property exists to convey is gone"
    );
}

/// Every entry names a real, readable parameter.
///
/// A decoder that mis-strided the array would still return plausible-looking
/// ids; reading each one back is what catches that. The 24-byte
/// `AudioUnitParameter` stride (8-byte handle + three `u32`s + padding) is easy
/// to get wrong as 16.
#[test]
fn every_overview_entry_addresses_a_readable_parameter() {
    let _g = lock();
    let au = MATRIX_REVERB.open(48_000.0, 512);

    let overview = au.parameters_for_overview().expect("overview");
    for p in &overview {
        assert_eq!(
            p.scope,
            tutti_au_host::types::K_AUDIO_UNIT_SCOPE_GLOBAL,
            "parameter {} came back in scope {}, not global — a mis-strided \
             decode reads a neighbouring field as the scope",
            p.id,
            p.scope
        );
        assert!(
            au.parameters().get(p.id).is_ok(),
            "overview names parameter {} but it cannot be read",
            p.id
        );
    }
}

/// A unit that does not implement the property reports that, rather than
/// inventing an empty list.
///
/// `Ok(vec![])` and `Err(InvalidProperty)` mean different things to a host: the
/// first says "I curate nothing", the second "ask my parameter list instead".
/// Collapsing them would silently downgrade every non-implementer.
#[test]
fn a_non_implementing_unit_reports_the_gap_rather_than_an_empty_list() {
    let _g = lock();
    let au = DLS_SYNTH.open(48_000.0, 512);

    match au.parameters_for_overview() {
        Err(AuError::OsStatus { code, .. }) if code == INVALID_PROPERTY => {}
        Err(other) => panic!(
            "expected OSStatus {INVALID_PROPERTY} (InvalidProperty) from a \
             non-implementer, got {other:?}"
        ),
        Ok(list) => panic!(
            "DLSMusicDevice answered ParametersForOverview with {} entries; if \
             it started implementing the property this test needs a different \
             non-implementer, not deleting",
            list.len()
        ),
    }
}

/// `nick_name` and `icon_location` report a missing property as `Err`, not as
/// `Ok(None)`.
///
/// The same distinction `parameters_for_overview` already had a test for, and
/// for the same reason: `Ok(None)` says "implements it, nothing set", `Err`
/// says "ask elsewhere". Collapsing them downgrades every non-implementer
/// silently. Both readers survived being rewritten to swallow their error
/// before this existed.
#[test]
fn the_string_readers_report_a_missing_property_as_an_error() {
    let _g = lock();
    // `icon_location` is witnessed by the real corpus: 16 units refuse it, so
    // the sweep must run to completion rather than return on the first refusal.
    // Returning early can only ever reach one of the two readers.
    let mut icon_refusals = 0;
    for info in every_component() {
        let Ok(au) = std::panic::catch_unwind(|| open_info(&info, 48_000.0, 512)) else {
            continue;
        };
        if let Err(e) = au.icon_location() {
            assert!(
                matches!(e, AuError::OsStatus { code, .. } if code == INVALID_PROPERTY),
                "{}: icon_location failed with {e:?}, expected InvalidProperty",
                info.name
            );
            icon_refusals += 1;
        }
    }
    assert!(
        icon_refusals > 0,
        "no unit refused IconLocation, so this cannot distinguish an \
         Err-propagating reader from one that swallows the error into Ok(None)"
    );

    // `nick_name` needs the probe. **Every** AU on this machine answers
    // NickName, so sweeping the corpus for a refusal asserts a fact about the
    // installed units rather than about the reader, and fails the day the last
    // refuser stops refusing — which is how this test broke. The probe declines
    // every property it does not implement, so it witnesses the path directly.
    // Same reasoning as `Misbehaviour::RefusesLatency`, which exists because no
    // real AU refuses latency either.
    let probe = Misbehaviour::None.open(48_000.0, 512);
    match probe.nick_name() {
        Err(AuError::OsStatus { code, .. }) if code == INVALID_PROPERTY => {}
        Err(other) => panic!(
            "expected OSStatus {INVALID_PROPERTY} (InvalidProperty) from a \
             probe that does not implement NickName, got {other:?}"
        ),
        Ok(name) => panic!(
            "the probe answered NickName with {name:?}; if it grew the property \
             this test needs a different non-implementer, not deleting"
        ),
    }
}

/// The overview decode reports every entry the AU wrote, and each entry's
/// address is decoded field-for-field.
///
/// Pins the two mutations the existential subset/order tests miss: dropping the
/// last entry (they only check what is present), and corrupting `element`
/// (nothing else asserts it). The counts are the AU's own, re-derived from the
/// property size rather than hardcoded, so this tracks the machine.
#[test]
fn the_overview_decodes_every_entry_the_au_reported() {
    let _g = lock();
    for unit in [MATRIX_REVERB, DYNAMICS] {
        let au = unit.open(48_000.0, 512);
        let overview = au.parameters_for_overview().expect("overview");

        // The AU's own byte count, decoded independently of the module under test.
        let mut size: u32 = 0;
        let mut writable: u8 = 0;
        let st = unsafe {
            coreaudio_sys::AudioUnitGetPropertyInfo(
                au.raw_unit(),
                tutti_au_host::types::K_AUDIO_UNIT_PROPERTY_PARAMETERS_FOR_OVERVIEW,
                tutti_au_host::types::K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &mut size,
                &mut writable,
            )
        };
        assert_eq!(st, 0, "{}: GetPropertyInfo failed", unit.label);
        let stride = std::mem::size_of::<tutti_au_host::types::AudioUnitParameter>() as u32;
        assert_eq!(
            overview.len() as u32,
            size / stride,
            "{}: decoded {} entries but the AU reported {} bytes ({} entries) — \
             a dropped or phantom trailing entry",
            unit.label,
            overview.len(),
            size,
            size / stride
        );

        for p in &overview {
            assert_eq!(
                p.element, 0,
                "{}: parameter {} decoded element {}; every global-scope entry \
                 on this corpus is element 0, so a non-zero value means the \
                 field was read from the wrong offset",
                unit.label, p.id, p.element
            );
        }
    }
}

/// Most units point at an icon file, and the path they give exists on disk.
///
/// A URL that does not resolve is worse than none — a browser row would show a
/// broken image. Asserting existence is what separates "the AU answered" from
/// "the answer is usable".
#[test]
fn an_icon_url_names_a_file_that_exists() {
    let _g = lock();
    let mut with_icon = 0;
    let mut checked = 0;
    for info in every_component() {
        let Ok(au) = std::panic::catch_unwind(|| open_info(&info, 48_000.0, 512)) else {
            continue;
        };
        checked += 1;
        if let Ok(Some(path)) = au.icon_location() {
            assert!(
                std::path::Path::new(&path).exists(),
                "{}: icon path {path:?} does not exist",
                info.name
            );
            with_icon += 1;
        }
    }
    assert!(
        with_icon > 0,
        "no unit of the {checked} instantiated offered an icon; measured 36 on \
         macOS 15.6, so this is a harness or decode failure rather than a \
         change in the corpus"
    );
}

/// `every_component` hides this harness's probes from the corpus sweeps.
///
/// The sweeps in this file assert facts about *the units installed on this
/// machine*. `AudioComponentRegister` puts each probe in the same process-wide
/// registry the sweep walks, so once any test in this binary opens one, every
/// later sweep can see it — and Rust runs those tests on several threads, so
/// whether it did depended on which thread won. That is what made
/// `every_unit_round_trips_a_context_name` fail about one run in five: it found
/// `probe that fails ClassInfo`, whose whole purpose is to refuse properties,
/// and reported the deliberate refusal as an installed unit's bug.
///
/// This opens a probe *first*, so the registry definitely contains one, and then
/// asserts the sweep does not see it. Without the filter this fails outright
/// rather than intermittently.
#[test]
fn the_corpus_sweep_never_sees_a_harness_probe() {
    let _g = lock();

    // Force the probes into the registry — this is the exact precondition that
    // made the race reachable.
    let _probe = Misbehaviour::None.open(48_000.0, 512);

    let probes: Vec<_> = every_component()
        .into_iter()
        .filter(|c| c.manufacturer_code == PROBE_MANUFACTURER)
        .map(|c| c.name)
        .collect();

    assert!(
        probes.is_empty(),
        "every_component() returned harness probes: {probes:?}; a corpus sweep \
         would report their deliberate refusals as installed-unit failures"
    );
}
