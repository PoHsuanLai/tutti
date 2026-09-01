//! Can this host tell an AU it cannot load from one that merely failed?
//!
//! `AudioComponent.h:498-502` — `AudioComponentInstantiate` "must be used to
//! instantiate any component with kAudioComponentFlag_RequiresAsyncInstantiation
//! set in its component flags", and `:200-201` says the system sets that flag
//! automatically for "v3 audio units with views".
//!
//! This host only ever called `AudioComponentInstanceNew`, and dropped
//! `componentFlags` when building [`AuComponentInfo`] — so it could neither
//! avoid the case nor report it. What a caller got was
//! `kAudioUnitErr_CannotDoInCurrentContext` (-10863), rendered as "cannot do in
//! current context": a message that reads like a transient condition worth
//! retrying, for a component this entry point can never create.
//!
//! # The corpus, not a fixture
//!
//! These run against whatever is installed. That is deliberate — the flag is
//! set by the *system* at registration, not by anything a test can construct,
//! so a synthetic component cannot carry it. The trade is that coverage depends
//! on the machine: on a system with no v3-with-view units the flag-specific
//! assertions have nothing to look at, and say so rather than passing silently.

#![cfg(target_os = "macos")]

use tutti_au_host::component::{enumerate_components, AuComponentInfo};
use tutti_au_host::AuError;
use tutti_au_host::AuHandle;

/// Every component the system advertises.
fn corpus() -> Vec<AuComponentInfo> {
    enumerate_components()
}

/// Components the system says need asynchronous instantiation.
fn async_only(all: &[AuComponentInfo]) -> Vec<&AuComponentInfo> {
    all.iter()
        .filter(|c| c.requires_async_instantiation())
        .collect()
}

/// The flag survives enumeration instead of being dropped.
///
/// `componentFlags` was read into the local description and then discarded, so
/// nothing above `enumerate_components` could see it. Without this the host
/// cannot distinguish a component it must skip from one it should try.
#[test]
fn enumeration_carries_the_component_flags() {
    let all = corpus();
    assert!(!all.is_empty(), "no AudioComponents on this system");

    // Some flag bit is set somewhere in a corpus this size — the system sets
    // `SandboxSafe` and `IsV3AudioUnit` routinely. An all-zero result across
    // every component means the field is not being read at all.
    let any_flags = all.iter().any(|c| c.flags != 0);
    assert!(
        any_flags,
        "every one of the {} components reports flags == 0, so componentFlags \
         is being dropped rather than carried",
        all.len()
    );
}

/// A component requiring async instantiation is refused with a specific error.
///
/// The point of the finding: the refusal names the reason, rather than handing
/// back a generic "cannot do in current context" that suggests retrying.
#[test]
fn an_async_only_component_is_refused_by_name() {
    let all = corpus();
    let async_only = async_only(&all);

    if async_only.is_empty() {
        // Not an assertion, because it would be one that cannot fail on a
        // machine with no such units. Reported so a green run on such a machine
        // is not mistaken for coverage.
        eprintln!(
            "no component on this system sets RequiresAsyncInstantiation \
             (checked {}); the refusal path is unexercised here",
            all.len()
        );
        return;
    }

    for info in async_only {
        // SAFETY: the handle came from `AudioComponentFindNext` via enumeration.
        let err = unsafe { AuHandle::new(info.component) }
            .err()
            .unwrap_or_else(|| {
                panic!(
                    "{}: AudioComponentInstanceNew must not create a component \
                     the system flagged as async-only",
                    info.name
                )
            });

        assert!(
            matches!(err, AuError::RequiresAsyncInstantiation),
            "{}: refused with {err:?}, not the variant that names why — a \
             caller cannot tell this from a transient failure",
            info.name
        );
    }
}

/// A component *without* the flag is not refused by the new guard.
///
/// The negative half. Without it, a guard that refused everything would pass
/// the test above while making the host unable to load any AU at all.
#[test]
fn a_normal_component_is_not_refused_as_async_only() {
    let all = corpus();
    let normal: Vec<_> = all
        .iter()
        .filter(|c| !c.requires_async_instantiation())
        .collect();
    assert!(
        !normal.is_empty(),
        "every component on this system is async-only, which cannot be right"
    );

    // One is enough to show the guard is not universal, and instantiating the
    // whole corpus is slow. A component may still fail for its own reasons —
    // what matters is that it is not refused by *this* guard.
    let mut tried = 0;
    for info in normal.iter().take(20) {
        // SAFETY: the handle came from `AudioComponentFindNext` via enumeration.
        match unsafe { AuHandle::new(info.component) } {
            Ok(_) => {
                tried += 1;
                break;
            }
            Err(AuError::RequiresAsyncInstantiation) => panic!(
                "{}: refused as async-only, but the system did not set the flag",
                info.name
            ),
            // Any other failure is the component's business, not this guard's.
            Err(_) => continue,
        }
    }
    assert_eq!(
        tried, 1,
        "none of the first 20 non-async components could be instantiated at \
         all, so this test proved nothing about the guard"
    );
}

/// `is_v3` and `requires_async_instantiation` are different questions.
///
/// The system sets `IsV3AudioUnit` for every v3 unit and the async flag only
/// for those *with views*, so conflating them would refuse loadable v3 units.
/// Reported rather than asserted: whether the distinguishing case exists
/// depends on what is installed.
#[test]
fn v3_and_async_only_are_not_the_same_set() {
    let all = corpus();
    let v3: Vec<_> = all.iter().filter(|c| c.is_v3()).collect();
    let async_only = async_only(&all);

    eprintln!(
        "corpus: {} components, {} v3, {} async-only",
        all.len(),
        v3.len(),
        async_only.len()
    );

    // Every async-only component is v3 — the flag is only set for v3 units
    // with views — so this direction holds whenever the set is non-empty.
    for info in &async_only {
        assert!(
            info.is_v3(),
            "{}: requires async instantiation but is not flagged v3, which \
             contradicts AudioComponent.h:200-201",
            info.name
        );
    }
}
