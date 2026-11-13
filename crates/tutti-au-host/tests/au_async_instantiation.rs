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
//! //! # A component that carries the flag
//!
//! The flag is part of a component's description, so
//! `an_async_only_component_is_refused_by_name` registers the shared test plugin
//! a second time under a subtype whose description sets it
//! (`tutti_shared_plugin::register_au_async_only`) and asserts the refusal on
//! that unit as well as on every component the system flags itself.

#![cfg(target_os = "macos")]

use tutti_au_host::AuError;
use tutti_au_host::AuHandle;
use tutti_au_host::component::{AuComponentInfo, enumerate_components};

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
    tutti_shared_plugin::register_au_async_only();
    let all = corpus();
    let async_only = async_only(&all);
    assert!(
        async_only
            .iter()
            .any(|c| c.sub_type == u32::from_be_bytes(tutti_shared_plugin::AU_ASYNC_SUBTYPE)),
        "the registered async-only copy of the shared plugin is not in the \
         enumeration with its flag, so no component here can be refused"
    );

    for info in async_only {
        let err = AuHandle::new(info.component).err().unwrap_or_else(|| {
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
        match AuHandle::new(info.component) {
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
