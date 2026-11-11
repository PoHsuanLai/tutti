//! The host contract: the same cases against every plugin format, driven
//! through `Loaded`, `Processor`, `Controller` and `Control` on the format's
//! real host and the one test plugin (`tutti-shared-plugin`).
//!
//! A format is a [`Format`] and a test file,
//! `contract!(Format, gaps = N, hostings = [InProcess, OutOfProcess])`. The
//! expected figures are in [`spec`], not imported from the plugin. A case a
//! format cannot pass is a [`Gap`]: it must fail with the stated text, and `N`
//! must equal the number of gaps, so none is added unnoticed. `hostings` names
//! the markers the format's loaded type implements (design 021 §3.3): each one
//! listed is checked at compile time, and `scripts/check-hosting.sh` fails an
//! impl the list leaves out.

mod active;
mod cases;
mod format;
mod formats;
mod plugin;
pub mod spec;
mod transcript;

pub use active::{Active, BLOCK, RATE, Render, Width, prepare};
pub use assert_no_alloc::AllocDisabler;
pub use cases::*;
pub use format::{Format, Gap, ParamTiming, run};
#[cfg(target_os = "macos")]
pub use formats::Au;
pub use formats::{Clap, Vst2, Vst3, Vst3Cpp};
pub use plugin::{bundle_of, clap as clap_path, library, vst3_cpp_module, vst3_module};
pub use transcript::{
    Kind, Logged, Told, Transcript, find_blob, patch_level, patch_mode, replace_blob, select,
    set_level, set_mode, transcript,
};
pub use tutti_plugin_types::{InProcess, OutOfProcess};

/// The case list, once: a test per case, and the names a [`Gap`] may use.
#[macro_export]
macro_rules! contract {
    ($fmt:ty, gaps = $gaps:expr, hostings = [$($hosting:ident),+ $(,)?]) => {
        // Mutation (run): list `OutOfProcess` for `Vst2` → E0277, does not
        // compile.
        const _: fn() = || {
            $({
                fn hosted<T: $crate::$hosting>() {}
                hosted::<<$fmt as $crate::Format>::L>();
            })+
        };
        $crate::contract!(@each $fmt, $gaps,
            routing_tag_oracle,
            reconfigure_keeps_rendering,
            buses_reported,
            latency_reported,
            latency_applied,
            reset_clears_the_delay_line,
            tail_reported,
            the_control_surface_answers_alike_loaded_and_active,
            parameters_listed,
            parameter_round_trip,
            unlisted_parameter_is_none,
            active_write_applied,
            automation_point_offset,
            note_at_offset,
            arrival_is_ordered_and_sized,
            transport_reaches_the_plugin,
            transport_change_is_told_at_its_frame,
            state_round_trip,
            loaded_write_reaches_the_audio,
            loaded_write_survives_a_save,
            reopen_is_a_fresh_instance,
            corrupt_state_leaves_plugin_usable,
            halves_of_two_activations_are_stuck,
            f64_carries_same_audio,
            overflowing_log_fails_the_oracle,
            process_does_not_allocate);
    };
    (@each $fmt:ty, $gaps:expr, $($case:ident),*) => {
        // One allocator per test binary, for `process_does_not_allocate`.
        #[global_allocator]
        static ALLOCATOR: $crate::AllocDisabler = $crate::AllocDisabler;

        $( #[test] fn $case() { $crate::run::<$fmt>(stringify!($case), $crate::$case::<$fmt>) } )*

        // Mutation (run): `gaps = 1` over an empty `GAPS` fails.
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
