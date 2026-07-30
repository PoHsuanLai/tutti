//! Shared plugin-classification vocabulary.
//!
//! Each native format reports a category/type taxonomy. These neutral mirrors
//! live here so both the format host crate (which maps its native SDK enum into
//! one, e.g. `tutti-vst2-host` from the `vst` crate's `Category`) and the
//! catalog/wire layer in `tutti-plugin` (which embeds it in `PluginClass`)
//! speak one type instead of each keeping a private copy plus a hand-written
//! cross-walk.
//!
//! Distinct from [`crate::metadata`], which holds the *engine-wiring* load data
//! (bus widths, latency); this is the *classification* taxonomy.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The plugin's declared VST2 category — a neutral mirror of the `vst` crate's
/// `Category`, owned here so neither the host crate's public API nor the wire
/// vocab leaks the `vst` dependency. `tutti-vst2-host` maps the native value
/// into this; `tutti-plugin` embeds it in `PluginClass::Vst2`.
///
/// Three kinds of answer, kept apart:
///
/// - a named variant — the plugin returned a code VST 2.4 assigns a meaning to;
/// - [`Unrecognized`](Self::Unrecognized) — it returned something else, and the
///   number is carried so a caller can still see it;
/// - [`Unasked`](Self::Unasked) — nobody called `effGetPlugCategory`.
///
/// The last two used to be one bare `Unknown` that was also `#[default]`, so a
/// scan that never queried and a plugin that answered `kPlugCategUnknown` were
/// the same value. `kPlugCategUnknown` is itself a real answer (0), and it maps
/// to `Unrecognized(0)` rather than to `Unasked` — the plugin did reply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Vst2Category {
    /// Nobody asked. The default, so a descriptor built by struct-update claims
    /// nothing rather than claiming the plugin declined to classify itself.
    #[default]
    Unasked,
    /// The plugin answered with a code this enum does not name, carried
    /// verbatim. Includes `kPlugCategUnknown` (0), which is a plugin saying it
    /// has no category — an answer, not a silence.
    Unrecognized(i32),
    Effect,
    Synth,
    Analysis,
    Mastering,
    Spacializer,
    RoomFx,
    SurroundFx,
    Restoration,
    OfflineProcess,
    Shell,
    Generator,
}

impl Vst2Category {
    /// `true` when a plugin answered at all, whether or not the code is named.
    pub fn was_answered(&self) -> bool {
        !matches!(self, Self::Unasked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Nobody asked" and "the plugin said it has no category" are different
    /// answers, and both used to be a single bare `Unknown`.
    #[test]
    fn an_unasked_category_is_not_an_unrecognized_one() {
        assert_ne!(Vst2Category::Unasked, Vst2Category::Unrecognized(0));
        assert!(!Vst2Category::Unasked.was_answered());
        assert!(Vst2Category::Unrecognized(0).was_answered());
        assert!(Vst2Category::Effect.was_answered());
    }

    /// The raw code survives, so a caller can see what the plugin actually
    /// returned instead of a flattened `Unknown`. Two unnamed codes stay
    /// distinct from each other.
    #[test]
    fn an_unrecognized_category_keeps_its_code() {
        assert_ne!(
            Vst2Category::Unrecognized(42),
            Vst2Category::Unrecognized(99)
        );
        match Vst2Category::Unrecognized(42) {
            Vst2Category::Unrecognized(code) => assert_eq!(code, 42),
            other => panic!("expected Unrecognized, got {other:?}"),
        }
    }

    /// A default-constructed category claims nothing. `Unasked` is the default
    /// precisely because a descriptor built by struct-update has not queried.
    #[test]
    fn the_default_category_claims_nothing() {
        assert_eq!(Vst2Category::default(), Vst2Category::Unasked);
        assert!(!Vst2Category::default().was_answered());
    }

    /// Both new shapes survive the bincode wire, and an `Unrecognized` payload
    /// arrives with its code intact — a decode that dropped it would restore
    /// the flattening this variant exists to prevent.
    #[cfg(feature = "serde")]
    #[test]
    fn the_category_survives_the_bincode_round_trip() {
        for want in [
            Vst2Category::Unasked,
            Vst2Category::Unrecognized(0),
            Vst2Category::Unrecognized(-7),
            Vst2Category::Synth,
        ] {
            let bytes = bincode::serialize(&want).expect("serialize");
            let back: Vst2Category = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(back, want);
        }
    }
}
