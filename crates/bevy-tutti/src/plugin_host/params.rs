//! A hosted plugin's parameter values, mirrored on its entity, and its GUI
//! edits reaching modulation lanes (doc 014 §8.2).
//!
//! A plugin parameter with a modulation lane has a knob of its own: the lane
//! sends `knob + Σ offsets` into the plugin's event input, so the plugin's
//! own value is no longer what plays. The knob starts at the plugin's value
//! when the lane is built, read from [`PluginParamValues`], and a value the
//! plugin reports later (an edit in its GUI, through `ParameterChangeSink`)
//! is written into the lane's knob ([`plugin_param_edits`]).

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
    /// The callback is the handle's one slot (`on_parameter_changed`
    /// replaces any other).
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
        // overwritten by a value read earlier.
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
