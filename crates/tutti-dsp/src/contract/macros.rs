//! `design_contract!` and `realization_contract!`.

/// Makes one `#[test]` per case of the design contract for `$design`, an
/// expression of a [`DesignCase`](crate::contract::DesignCase) type, then the
/// cases of each group named after it: `Lti` (a
/// [`LtiCase`](crate::contract::LtiCase)) and `reverb` (a
/// [`ReverbCase`](crate::contract::ReverbCase)).
///
/// ```text
/// design_contract!(MyDesign);               // the core
/// design_contract!(MyDesign, Lti, reverb);  // and two groups
/// ```
///
/// Invoke it where a design's own tests are, once a module: the tests have fixed
/// names.
#[macro_export]
macro_rules! design_contract {
    ($design:expr $(, $group:ident)* $(,)?) => {
        #[test]
        fn contract_bounds_dominate() {
            $crate::contract::cases::design::bounds_dominate(&$design).unwrap();
        }
        #[test]
        fn contract_pure() {
            $crate::contract::cases::design::pure(&$design).unwrap();
        }
        #[test]
        fn contract_vacuity() {
            $crate::contract::cases::design::vacuity(&$design).unwrap();
        }
        $( $crate::__design_group!($group, $design); )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __design_group {
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
            $crate::contract::cases::design::anchors(&$design).unwrap();
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

/// Makes one `#[test]` per case of the realization contract for `$design`, an
/// expression of a [`RealizationCase`](crate::contract::RealizationCase) type,
/// then the cases of each group named after it (`Lti`, `reverb`), as
/// [`design_contract!`] does.
#[macro_export]
macro_rules! realization_contract {
    ($design:expr $(, $group:ident)* $(,)?) => {
        #[test]
        fn realization_contract_latency() {
            $crate::contract::cases::realization::latency(&$design).unwrap();
        }
        #[test]
        fn realization_contract_arrival() {
            $crate::contract::cases::realization::arrival(&$design).unwrap();
        }
        #[test]
        fn realization_contract_click_free() {
            $crate::contract::cases::realization::click_free(&$design).unwrap();
        }
        #[test]
        fn realization_contract_reset() {
            $crate::contract::cases::realization::reset(&$design).unwrap();
        }
        #[test]
        fn realization_contract_fresh() {
            $crate::contract::cases::realization::fresh(&$design).unwrap();
        }
        #[test]
        fn realization_contract_block_independent() {
            if $crate::contract::cases::realization::block_independence_required(&$design) {
                $crate::contract::cases::realization::block_independent(&$design).unwrap();
            }
        }
        #[test]
        fn realization_contract_landed() {
            $crate::contract::cases::realization::landed(&$design).unwrap();
        }
        #[test]
        fn realization_contract_stream_clock() {
            if $crate::contract::cases::realization::block_independence_required(&$design) {
                $crate::contract::cases::realization::stream_clock(&$design).unwrap();
            }
        }
        #[test]
        fn realization_contract_vacuity() {
            $crate::contract::cases::realization::vacuity(&$design).unwrap();
        }
        $( $crate::__realization_group!($group, $design); )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __realization_group {
    (Lti, $design:expr) => {
        #[test]
        fn realization_contract_tail_covers() {
            $crate::contract::cases::realization::tail_covers(&$design).unwrap();
        }
        #[test]
        fn realization_contract_stability() {
            $crate::contract::cases::realization::stability(&$design).unwrap();
        }
        #[test]
        fn realization_contract_linear() {
            $crate::contract::cases::realization::linear(&$design).unwrap();
        }
        #[test]
        fn realization_contract_response() {
            $crate::contract::cases::realization::response(&$design).unwrap();
        }
        #[test]
        fn realization_contract_decay() {
            $crate::contract::cases::realization::decay(&$design).unwrap();
        }
        #[test]
        fn realization_contract_tail_tight() {
            $crate::contract::cases::realization::tail_tight(&$design).unwrap();
        }
    };
    (reverb, $design:expr) => {
        #[test]
        fn realization_contract_rt60() {
            $crate::contract::cases::realization::rt60(&$design).unwrap();
        }
        #[test]
        fn realization_contract_band_order() {
            $crate::contract::cases::realization::band_order(&$design).unwrap();
        }
        #[test]
        fn realization_contract_wet_only() {
            $crate::contract::cases::realization::wet_only(&$design).unwrap();
        }
        #[test]
        fn realization_contract_decorrelated() {
            $crate::contract::cases::realization::decorrelated(&$design).unwrap();
        }
        #[test]
        fn realization_contract_line_lengths() {
            $crate::contract::cases::realization::line_lengths(&$design).unwrap();
        }
        #[test]
        fn realization_contract_pre_delay() {
            $crate::contract::cases::realization::pre_delay(&$design).unwrap();
        }
        #[test]
        fn realization_contract_size() {
            $crate::contract::cases::realization::size(&$design).unwrap();
        }
        #[test]
        fn realization_contract_diffusion() {
            $crate::contract::cases::realization::diffusion(&$design).unwrap();
        }
        #[test]
        fn realization_contract_freeze() {
            $crate::contract::cases::realization::freeze(&$design).unwrap();
        }
        #[test]
        fn realization_contract_width() {
            $crate::contract::cases::realization::width(&$design).unwrap();
        }
        #[test]
        fn realization_contract_wet_gain() {
            $crate::contract::cases::realization::wet_gain(&$design).unwrap();
        }
        #[test]
        fn realization_contract_reverb_vacuity() {
            $crate::contract::cases::realization::reverb_vacuity(&$design).unwrap();
        }
    };
}
