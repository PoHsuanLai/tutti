//! Cross-loader `ParameterInfo` builders.
//!
//! The builders now live in `tutti-plugin-types` (so any crate implementing
//! `PluginFormatHost` can build a `ParameterInfo` without depending on
//! `tutti-plugin`); re-exported here so the existing call sites keep working.

pub(crate) use tutti_plugin::server::{make_param_info, ALL_AUTOMATABLE};
