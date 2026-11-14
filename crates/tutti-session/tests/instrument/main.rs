//! The instrument contract against `PolySynth`, the SoundFont unit and a hosted
//! plugin, and the instruments' main pair and refusals. The binary installs
//! `AllocDisabler`, so an allocation inside an `assert_no_alloc` closure aborts.
//!
//! The plugin subject needs a built `plugin-server`, and fails without it.

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[macro_use]
mod instrument_support;

mod main_pair;
mod plugin;
mod polysynth;
mod refusal;
mod soundfont;
