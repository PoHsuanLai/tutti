//! A hosted plugin's parameter values, mirrored on its entity, and its GUI
//! edits reaching modulation lanes (doc 014 §8.2).
//!
//! A plugin parameter with a modulation lane has a knob of its own: the lane
//! sends `knob + Σ offsets` into the plugin's event input, so the plugin's
//! own value is no longer what plays. The knob starts at the plugin's value
//! when the lane is built, read from [`PluginParamValues`], and a value the
//! plugin reports later (an edit in its GUI, through `ParameterChangeSink`)
//! is written into the lane's knob ([`plugin_param_edits`]).
//!
//! The mirror and its read are `tutti-plugin`'s ([`PluginParamValues`],
//! `ParamRead`); this module runs the read on the [`AsyncComputeTaskPool`]
//! ([`read_plugin_params`]) and lands it.
//!
//! **Reports are VST2's only, today.** Of the formats, only VST2's
//! `audioMasterAutomate` reaches the parameter callback (in process, and
//! through the plugin-server's `ParameterChanged`). CLAP, VST3 and AU emit no
//! per-value report through the server (`tutti-plugin-server`'s
//! `Plugin::poll_async_events`): a GUI edit on one of them changes the
//! plugin's own value, which the lane's next delivery replaces, and the knob
//! does not move.
//!
//! **An echo is not an edit.** A plugin that reports the value the lane just
//! set (from inside `setParameter`) would make that modulated value the knob,
//! and the next delivery would add the offsets again, ratcheting the knob to
//! the clamp. The VST2 host drops a report made inside its own
//! `setParameter` call (`tutti-vst2-host`'s `host_write`), so none reaches
//! here; a format that reports values in future needs the same filter.

use std::collections::HashMap;

use bevy_ecs::prelude::*;
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use tutti_plugin::handles::PluginHandle;
pub use tutti_plugin::PluginParamValues;

use crate::modulation::{ModulationState, ParamRef};

/// A plugin's load-time parameter read, in flight on the
/// [`AsyncComputeTaskPool`] (each value is a blocking round trip). Its
/// [`PluginParamValues`] is on the same entity; [`plugin_param_edits`]
/// lands the read into it and removes this.
#[derive(Component)]
pub struct PluginParamRead(Task<HashMap<u32, f32>>);

/// The mirror for `handle` and its read, started on the
/// [`AsyncComputeTaskPool`]: insert both on the plugin's entity.
///
/// [`PluginParamValues::start`] takes the handle's single
/// `on_parameter_changed` slot (see there).
pub fn read_plugin_params(handle: &PluginHandle) -> (PluginParamValues, PluginParamRead) {
    let (mirror, read) = PluginParamValues::start(handle);
    let task = AsyncComputeTaskPool::get().spawn(async move { read.run() });
    (mirror, PluginParamRead(task))
}

/// Lands each finished load-time read, then applies the values the plugins
/// reported since the last frame: into the mirror, and into the knob of the
/// lane on that parameter when it has one (a GUI edit becomes the knob, doc
/// 014 §8.2). Under automation the curve still decides the base: the knob
/// shows again when the take or the automation goes.
///
/// Edits wait for the read (`PluginParamValues::drain_edits` says why).
pub fn plugin_param_edits(
    mut commands: Commands,
    mut plugins: Query<(Entity, &mut PluginParamValues, Option<&mut PluginParamRead>)>,
    state: Option<Res<ModulationState>>,
) {
    for (entity, mut mirror, reading) in &mut plugins {
        let mirror = mirror.bypass_change_detection();
        if let Some(mut reading) = reading {
            if let Some(values) = block_on(future::poll_once(&mut reading.0)) {
                mirror.land(values);
                commands.entity(entity).remove::<PluginParamRead>();
            }
        }
        mirror.drain_edits(|id, value| {
            if let Some(state) = state.as_ref() {
                state.set_knob(ParamRef::plugin(entity, id), value);
            }
        });
    }
}
