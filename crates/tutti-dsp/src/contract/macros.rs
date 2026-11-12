//! `family_contract!` and `recurrence_contract!`.

/// Makes one `#[test]` per case of the family contract for `$design`, an
/// expression of a [`FamilyCase`](crate::contract::FamilyCase) type, then the
/// cases of each group named after it: `Grows` (a [`Grows`](crate::Grows) family:
/// its `reach` covers the `need` of every member), `Lti` (a
/// [`LtiCase`](crate::contract::LtiCase)) and `reverb` (a
/// [`ReverbCase`](crate::contract::ReverbCase)).
///
/// ```text
/// family_contract!(MyFamily);                      // the core
/// family_contract!(MyFamily, Grows, Lti, reverb);  // and three groups
/// ```
///
/// Invoke it where a family's own tests are, once a module: the tests have fixed
/// names.
#[macro_export]
macro_rules! family_contract {
    ($design:expr $(, $group:ident)* $(,)?) => {
        #[test]
        fn contract_pure() {
            $crate::contract::cases::design::pure(&$design).unwrap();
        }
        #[test]
        fn contract_vacuity() {
            $crate::contract::cases::design::vacuity(&$design).unwrap();
        }
        $( $crate::__family_group!($group, $design); )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __family_group {
    (Grows, $design:expr) => {
        #[test]
        fn contract_reach_covers_need() {
            $crate::contract::cases::design::reach_covers_need(&$design).unwrap();
        }
    };
    (Lti, $design:expr) => {
        #[test]
        fn contract_stable() {
            $crate::contract::cases::design::stable(&$design).unwrap();
        }
        #[test]
        fn contract_radius() {
            $crate::contract::cases::design::radius(&$design).unwrap();
        }
        #[test]
        fn contract_anchors() {
            $crate::contract::cases::design::figures(&$design).unwrap();
        }
        #[test]
        fn contract_norms() {
            $crate::contract::cases::design::norms(&$design).unwrap();
        }
        #[test]
        fn contract_ports() {
            $crate::contract::cases::design::ports(&$design).unwrap();
        }
        #[test]
        fn contract_lti_vacuity() {
            $crate::contract::cases::design::lti_vacuity(&$design).unwrap();
        }
    };
    (reverb, $design:expr) => {
        #[test]
        fn contract_rt60_matches() {
            $crate::contract::cases::design::rt60_matches(&$design).unwrap();
        }
        #[test]
        fn contract_band_order() {
            $crate::contract::cases::design::band_order(&$design).unwrap();
        }
        #[test]
        fn contract_tail_monotone() {
            $crate::contract::cases::design::tail_monotone(&$design).unwrap();
        }
        #[test]
        fn contract_lossless_iff_frozen() {
            $crate::contract::cases::design::lossless_iff_frozen(&$design).unwrap();
        }
        #[test]
        fn contract_density() {
            $crate::contract::cases::design::density(&$design).unwrap();
        }
        #[test]
        fn contract_stable_refuses_hostile() {
            $crate::contract::cases::design::stable_refuses_hostile(&$design).unwrap();
        }
        #[test]
        fn contract_no_direct_path() {
            $crate::contract::cases::design::no_direct_path(&$design).unwrap();
        }
        #[test]
        fn contract_pre_delay() {
            $crate::contract::cases::design::pre_delay(&$design).unwrap();
        }
        #[test]
        fn contract_no_rescale() {
            $crate::contract::cases::design::no_rescale(&$design).unwrap();
        }
        #[test]
        fn contract_reverb_vacuity() {
            $crate::contract::cases::design::reverb_vacuity(&$design).unwrap();
        }
    };
}

/// Makes one `#[test]` per case of the recurrence contract for `$design`, an
/// expression of a [`RecurrenceCase`](crate::contract::RecurrenceCase) type, then
/// the cases of each group named after it (`Lti`, `reverb`, `BlockIndependent`), as
/// [`family_contract!`] does.
///
/// The cases state the three laws of design 024 section 2.7.4, each a diagram that
/// commutes, and the macro is laid out by them:
///
/// 1. **The recurrence realizes the member**: what the render does is what the
///    member's queries say (latency, arrival, response, decay, tail, linearity,
///    stability and, for a reverb, its RT60s, bands and controls).
/// 2. **The block form is the frame form**: how the input is cut into blocks, and
///    where a glide's clock starts, changes nothing (`block_independent`,
///    `stream_clock`, `cadence`).
/// 3. **The glide lands**: a retarget approaches the member without a click and
///    comes to rest on it, and `reset` and `fresh` return to silence on that rest
///    (`landed`, `click_free`, `reset`, `fresh`).
#[macro_export]
macro_rules! recurrence_contract {
    ($design:expr $(, $group:ident)* $(,)?) => {
        $crate::__recurrence_realizes!($design $(, $group)*);
        $crate::__recurrence_blocks!($design $(, $group)*);
        $crate::__recurrence_glide!($design $(, $group)*);
    };
}

/// Law 1 of [`recurrence_contract!`]: the recurrence realizes the member.
#[doc(hidden)]
#[macro_export]
macro_rules! __recurrence_realizes {
    ($design:expr $(, $group:ident)*) => {
        #[test]
        fn recurrence_contract_latency() {
            $crate::contract::cases::realization::latency(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_arrival() {
            $crate::contract::cases::realization::arrival(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_vacuity() {
            $crate::contract::cases::realization::vacuity(&$design).unwrap();
        }
        $( $crate::__recurrence_realizes_group!($group, $design); )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __recurrence_realizes_group {
    (BlockIndependent, $design:expr) => {};
    (Lti, $design:expr) => {
        #[test]
        fn recurrence_contract_tail_covers() {
            $crate::contract::cases::realization::tail_covers(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_stability() {
            $crate::contract::cases::realization::stability(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_linear() {
            $crate::contract::cases::realization::linear(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_response() {
            $crate::contract::cases::realization::response(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_decay() {
            $crate::contract::cases::realization::decay(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_tail_tight() {
            $crate::contract::cases::realization::tail_tight(&$design).unwrap();
        }
    };
    (reverb, $design:expr) => {
        #[test]
        fn recurrence_contract_rt60() {
            $crate::contract::cases::realization::rt60(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_band_order() {
            $crate::contract::cases::realization::band_order(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_wet_only() {
            $crate::contract::cases::realization::wet_only(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_decorrelated() {
            $crate::contract::cases::realization::decorrelated(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_line_lengths() {
            $crate::contract::cases::realization::line_lengths(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_pre_delay() {
            $crate::contract::cases::realization::pre_delay(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_size() {
            $crate::contract::cases::realization::size(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_diffusion() {
            $crate::contract::cases::realization::diffusion(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_freeze() {
            $crate::contract::cases::realization::freeze(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_width() {
            $crate::contract::cases::realization::width(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_wet_gain() {
            $crate::contract::cases::realization::wet_gain(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_reverb_vacuity() {
            $crate::contract::cases::realization::reverb_vacuity(&$design).unwrap();
        }
    };
}

/// Law 2 of [`recurrence_contract!`]: the block form is the frame form.
#[doc(hidden)]
#[macro_export]
macro_rules! __recurrence_blocks {
    ($design:expr $(, $group:ident)*) => {
        #[test]
        fn recurrence_contract_cadence() {
            let groups = [$(stringify!($group)),*];
            $crate::contract::cases::realization::cadence(
                &$design,
                groups.contains(&"BlockIndependent"),
            )
            .unwrap();
        }
        $( $crate::__recurrence_blocks_group!($group, $design); )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __recurrence_blocks_group {
    (BlockIndependent, $design:expr) => {
        #[test]
        fn recurrence_contract_block_independent() {
            $crate::contract::cases::realization::block_independent(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_stream_clock() {
            $crate::contract::cases::realization::stream_clock(&$design).unwrap();
        }
    };
    (Lti, $design:expr) => {};
    (reverb, $design:expr) => {};
}

/// Law 3 of [`recurrence_contract!`]: the glide lands.
#[doc(hidden)]
#[macro_export]
macro_rules! __recurrence_glide {
    ($design:expr $(, $group:ident)*) => {
        #[test]
        fn recurrence_contract_landed() {
            $crate::contract::cases::realization::landed(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_click_free() {
            $crate::contract::cases::realization::click_free(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_reset() {
            $crate::contract::cases::realization::reset(&$design).unwrap();
        }
        #[test]
        fn recurrence_contract_fresh() {
            $crate::contract::cases::realization::fresh(&$design).unwrap();
        }
    };
}
