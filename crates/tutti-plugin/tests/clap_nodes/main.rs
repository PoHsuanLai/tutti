//! A hosted CLAP plugin as a graph node (feature `clap`).

#[cfg(feature = "clap")]
#[path = "../support/clap_probe.rs"]
mod clap_probe;
#[cfg(feature = "clap")]
#[path = "../support/param_probe.rs"]
mod param_probe;

mod clap_automation;
mod clap_crash_recovery;
mod clap_fork;
mod clap_live;
mod clap_pdc_alignment;
mod clap_ragged;
mod clap_rate_refused;
mod clap_transport;
mod plugin_param_delivery;
