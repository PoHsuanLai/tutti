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
use std::sync::{Arc, Mutex, PoisonError};

use bevy_ecs::prelude::*;
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use tutti_plugin::handles::ParamAddress;
use tutti_plugin::handles::PluginHandle;

use crate::modulation::{ModulationState, ParamRef};

/// Edits the plugin reported, in order, waiting for the next frame.
type Edits = Arc<Mutex<Vec<(u32, f32)>>>;

/// A loaded plugin's parameter values, by id: read once off the frame
/// thread when it loads (after its saved state), then kept current from the
/// values it reports (`PluginHandle::on_parameter_changed`).
///
/// A modulation lane on one of its parameters starts its knob here; until
/// the read lands ([`is_ready`](Self::is_ready)), such a target waits
/// unresolved.
#[derive(Component)]
pub struct PluginParamValues {
    values: Option<HashMap<u32, f32>>,
    reading: Option<Task<HashMap<u32, f32>>>,
    edits: Edits,
}

impl PluginParamValues {
    /// Starts reading `handle`'s values on a worker (the reads are blocking
    /// round trips) and installs its parameter callback, which queues every
    /// value the plugin reports for [`plugin_param_edits`].
    ///
    /// The callback takes the handle's single `on_parameter_changed` slot,
    /// replacing any callback installed before, and one installed after
    /// replaces it (the lane's knob then stops following the GUI).
    pub fn read(handle: &PluginHandle) -> Self {
        let edits: Edits = Arc::default();
        let queue = Arc::clone(&edits);
        handle.on_parameter_changed(move |id, value| {
            queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((id, value));
        });
        let handle = handle.clone();
        let reading = AsyncComputeTaskPool::get().spawn(async move {
            let params = handle.params();
            params
                .parameter_descriptors()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| {
                    let id = param_id(p.id)?;
                    Some((id, params.parameter_value(p.id)?))
                })
                .collect()
        });
        Self {
            values: None,
            reading: Some(reading),
            edits,
        }
    }

    /// Values known from the start, with no plugin behind them (a test's, or
    /// a host that mirrors values its own way). Edits arrive through
    /// [`report`](Self::report).
    pub fn known(values: impl IntoIterator<Item = (u32, f32)>) -> Self {
        Self {
            values: Some(values.into_iter().collect()),
            reading: None,
            edits: Arc::default(),
        }
    }

    /// Queues a value as if the plugin had reported it.
    pub fn report(&self, id: u32, value: f32) {
        self.edits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, value));
    }

    /// Whether the load-time read has landed.
    pub fn is_ready(&self) -> bool {
        self.values.is_some()
    }

    /// Parameter `id`'s value: `None` while the read is in flight, `Some(None)`
    /// for a parameter the plugin did not list.
    pub fn get(&self, id: u32) -> Option<Option<f32>> {
        self.values.as_ref().map(|v| v.get(&id).copied())
    }
}

/// The `u32` a [`ParamAddress`] names in `ParamAddr::Id`.
fn param_id(id: ParamAddress) -> Option<u32> {
    match id {
        ParamAddress::Opaque(id) => Some(id.get()),
        ParamAddress::Index(i) => u32::try_from(i).ok(),
    }
}

/// Lands each finished load-time read, then applies the values the plugins
/// reported since the last frame: into the mirror, and into the knob of the
/// lane on that parameter when it has one (a GUI edit becomes the knob, doc
/// 014 §8.2). Under automation the curve still decides the base: the knob
/// shows again when the take or the automation goes.
pub fn plugin_param_edits(
    mut plugins: Query<(Entity, &mut PluginParamValues)>,
    state: Option<Res<ModulationState>>,
) {
    for (entity, mut mirror) in &mut plugins {
        let mirror = mirror.bypass_change_detection();
        if let Some(task) = mirror.reading.as_mut() {
            if let Some(values) = block_on(future::poll_once(task)) {
                mirror.reading = None;
                mirror.values = Some(values);
            }
        }
        // Edits wait for the read: one applied before it lands would be
        // overwritten by a value read earlier. The other order has a window
        // of its own: an edit reported while the read is running, before
        // the read reaches its parameter, is applied after it and so
        // replaces the value read, though the read is newer. Only a value
        // changed again within that window without a report is affected
        // (each change the plugin reports arrives here in order), and the
        // next report corrects it.
        if mirror.values.is_none() {
            continue;
        }
        let edits =
            std::mem::take(&mut *mirror.edits.lock().unwrap_or_else(PoisonError::into_inner));
        for (id, value) in edits {
            if let Some(values) = mirror.values.as_mut() {
                values.insert(id, value);
            }
            if let Some(state) = state.as_ref() {
                state.set_knob(ParamRef::plugin(entity, id), value);
            }
        }
    }
}
