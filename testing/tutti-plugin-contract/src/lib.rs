//! The host contract: the same cases against every plugin format, driven
//! through `Loaded`, `Processor`, `Controller` and `Control` on the format's
//! real host and the one test plugin (`tutti-shared-plugin`).
//!
//! A format is a [`Format`] and a test file, `contract!(Format, gaps = N)`. The
//! expected figures are in [`spec`], not imported from the plugin. A case a
//! format cannot pass is a [`Gap`]: it must fail with the stated text, and `N`
//! must equal the number of gaps, so none is added unnoticed.

mod active;
mod cases;
mod format;
mod formats;
mod plugin;
pub mod spec;
mod transcript;

pub use active::{Active, BLOCK, RATE, Width, prepare};
pub use assert_no_alloc::AllocDisabler;
pub use cases::*;
pub use format::{Format, Gap, ParamTiming, run};
pub use formats::Clap;
pub use plugin::{clap as clap_path, library};
pub use transcript::{Kind, Logged, Transcript, patch_mode, set_mode, transcript};

/// The case list, once: a test per case, and the names a [`Gap`] may use.
#[macro_export]
macro_rules! contract {
    ($fmt:ty, gaps = $gaps:expr) => {
        $crate::contract!(@each $fmt, $gaps,
            routing_tag_oracle,
            reconfigure_keeps_rendering,
            latency_reported,
            latency_applied,
            tail_reported,
            parameters_listed,
            parameter_round_trip,
            unlisted_parameter_is_none,
            active_write_applied,
            automation_point_offset,
            note_at_offset,
            arrival_is_ordered_and_sized,
            state_round_trip,
            loaded_write_reaches_the_audio,
            loaded_write_survives_a_save,
            corrupt_state_leaves_plugin_usable,
            halves_of_two_activations_are_stuck,
            f64_carries_same_audio,
            process_does_not_allocate);
    };
    (@each $fmt:ty, $gaps:expr, $($case:ident),*) => {
        // One allocator per test binary, for `process_does_not_allocate`.
        #[global_allocator]
        static ALLOCATOR: $crate::AllocDisabler = $crate::AllocDisabler;

        $( #[test] fn $case() { $crate::run::<$fmt>(stringify!($case), $crate::$case::<$fmt>) } )*

        #[test]
        fn the_gaps_are_the_declared_ones() {
            use $crate::Format as _;
            let cases = [$(stringify!($case)),*];
            assert_eq!(<$fmt>::GAPS.len(), $gaps, "the number of declared gaps");
            for (name, gap) in <$fmt>::GAPS {
                assert!(cases.contains(name), "gap `{name}` ({}) names no case", gap.why);
            }
        }
    };
}
