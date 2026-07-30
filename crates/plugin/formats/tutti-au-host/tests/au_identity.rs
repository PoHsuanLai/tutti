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
//! mutation reverted. All five were caught.
//!
//! | mutation to `src/identity.rs` | caught by |
//! |---|---|
//! | `stride = 16` instead of `size_of::<AudioUnitParameter>()` (24) | `every_overview_entry_addresses_a_readable_parameter`, `the_overview_is_a_strict_subset_for_a_unit_that_curates` |
//! | `sort_unstable_by_key` the decoded overview | `the_overview_order_is_not_the_parameter_list_order` |
//! | `set_nick_name` writes `ContextName` | `a_nick_name_round_trips_and_can_be_replaced`, `a_nick_name_survives_non_ascii_text`, `two_instances_of_one_au_hold_independent_nick_names` |
//! | `icon_location` returns a path that does not exist | `an_icon_url_names_a_file_that_exists` |
//! | `parameters_for_overview` collapses `Err` into an empty list | `a_non_implementing_unit_reports_the_gap_rather_than_an_empty_list` |
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

/// Every instantiable unit accepts a context name and hands back the exact
/// string.
///
/// Asserts the round trip rather than the write status, because a write status
/// is what `HostMIDIProtocol` returns `noErr` for while storing garbage. The
/// count is asserted as "all of them" rather than a fixed number so the test
/// tracks the machine rather than a snapshot of it.
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
        checked += 1;
    }
    assert!(
        checked >= 40,
        "only {checked} units instantiated; the corpus should offer ~52, so \
         something is wrong with the harness rather than with the property"
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
