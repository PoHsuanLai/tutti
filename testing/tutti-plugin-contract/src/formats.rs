//! One [`Format`] per host: the whole of a format's side of the contract.

use crate::{Format, Gap, ParamTiming, plugin};

/// CLAP.
pub struct Clap;

impl Format for Clap {
    const NAME: &'static str = "clap";
    type L = tutti_clap_host::ClapLoaded;
    const PARAM_TIMING: ParamTiming = ParamTiming::SampleAccurate;
    const GAPS: &'static [(&'static str, Gap)] = &[];

    fn load() -> Self::L {
        tutti_clap_host::ClapLoaded::load(plugin::clap(), tutti_clap_host::LoadOptions::new())
            .expect("the plugin loads")
    }
}
