//! The instrument contract.
//!
//! The same cases against every instrument, through a session: the node is
//! added, made an instrument (`Session::add_instrument`, `Session::instrument`,
//! `Session::instrument_plugin`), played by its handle and rendered by an
//! offline session. A case a subject cannot pass is a [`Gap`]: it runs, and
//! must fail with the text the gap names, so a stale gap fails (the plugin
//! contract's rule). `gaps = N` in the test file is the ratchet.
//!
//! The expected figures are here, in the cases: a frame index the subject's
//! declaration implies, silence, a pitch in Hz. None is imported from a
//! subject, and no case compares a render with another render.
//!
//! # Mutations (run)
//!
//! Each was made, watched to fail the named tests, and restored:
//!
//! - `Rig::clock_of` returning the rendered count instead of the engine's
//!   clock (the frame skew of E6): `a_rate_change_keeps_the_note_on_its_frame`
//!   fails for `PolySynth` and the plugin, 18 to 21 frames late.
//! - `InstrumentHandle::send` ignoring its `At`: `note_on_sounds_at_its_stamp`
//!   and `a_beat_is_refused`.
//! - `InstrumentHandle::set_gain` storing nothing: `gain_reaches_the_output`.
//! - the strip reading outputs `[0, 1]` whatever the instrument names:
//!   `the_pair_a_node_names_is_the_one_played`.
//! - the role check skipped: `an_effect_plugin_is_refused`.
//! - the server not re-activating a plugin after a state restored before its
//!   first block: every plugin case (the plugin stays in its load-time mode),
//!   and the fork case alone 5 runs in 6.
//!
//! This lives under `tests/` and not in a crate: it needs `tutti-session`,
//! which only the umbrellas may depend on, and a dev-dependency is how a test
//! reaches it (`scripts/check-layout.sh`).
#![allow(dead_code, unused_imports, unused_macros)]

pub mod cases;
pub mod midi;
pub mod rig;
pub mod subjects;

pub use cases::{Gap, Subject};
pub use rig::{BLOCK, RATE, Rig};

/// Declares one `#[test]` per case for the subject `$subject`, and the
/// ratchet on its number of gaps.
macro_rules! contract {
    ($subject:ty, gaps = $gaps:expr) => {
        contract!(@each $subject,
            only_an_instrument_is_one,
            note_on_sounds_at_its_stamp,
            a_frame_already_past_plays_at_the_next_block,
            a_beat_is_refused,
            note_off_releases,
            velocity_zero_is_a_note_off,
            tail_covers_the_ring_out,
            all_sound_off_cuts_at_its_stamp,
            another_channel_is_heard_and_holds_nothing,
            gain_reaches_the_output,
            a_rate_change_keeps_the_note_on_its_frame,
            a_fork_starts_silent,
            sending_allocates_nothing);
        #[test]
        fn gaps_are_the_ratchet() {
            assert_eq!(
                <$subject as $crate::instrument_support::Subject>::GAPS.len(),
                $gaps,
                "a gap added or removed is a reviewed edit"
            );
        }
    };
    (@each $subject:ty, $($case:ident),*) => {
        $(
            #[test]
            fn $case() {
                $crate::instrument_support::cases::run::<$subject>(
                    stringify!($case),
                    $crate::instrument_support::cases::$case::<$subject>,
                );
            }
        )*
    };
}
