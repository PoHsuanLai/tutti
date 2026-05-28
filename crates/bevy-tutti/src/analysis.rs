//! Live analysis (pitch, transients, waveform, spectrum) mirror as a Bevy resource.

use std::sync::Arc;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::resources::AnalysisRes;

/// Trigger component: spawn an entity with this to enable live analysis.
///
/// Processed by `live_analysis_control_system`, calls `engine.enable_live_analysis()`.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub struct EnableLiveAnalysis;

/// Trigger component: spawn an entity with this to disable live analysis.
///
/// Processed by `live_analysis_control_system`, calls `engine.disable_live_analysis()`.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub struct DisableLiveAnalysis;

/// Live analysis state synced from Tutti via lock-free ArcSwap reads.
///
/// Fields are `Arc` pointers -- cheap to clone for UI consumption.
#[derive(Resource)]
pub struct LiveAnalysisData {
    pub pitch: Arc<tutti::analysis::PitchResult>,
    pub transients: Arc<Vec<tutti::analysis::Transient>>,
    pub waveform: Arc<tutti::analysis::WaveformSummary>,
    pub spectrum: Arc<tutti::analysis::SpectrumResult>,
    pub is_live: bool,
}

impl Default for LiveAnalysisData {
    fn default() -> Self {
        Self {
            pitch: Arc::new(tutti::analysis::PitchResult::default()),
            transients: Arc::new(Vec::new()),
            waveform: Arc::new(tutti::analysis::WaveformSummary::new(512)),
            spectrum: Arc::new(tutti::analysis::SpectrumResult::default()),
            is_live: false,
        }
    }
}

pub fn live_analysis_control_system(
    mut commands: Commands,
    analysis: Option<Res<AnalysisRes>>,
    mut data: ResMut<LiveAnalysisData>,
    enable_query: Query<Entity, Added<EnableLiveAnalysis>>,
    disable_query: Query<Entity, Added<DisableLiveAnalysis>>,
) {
    let Some(analysis) = analysis else { return };

    for entity in enable_query.iter() {
        if analysis.enable_live() {
            data.is_live = true;
            bevy_log::info!("Live analysis enabled");
        } else {
            bevy_log::warn!(
                "EnableLiveAnalysis: AnalysisHandle has no metering manager — \
                 analysis thread cannot start"
            );
        }
        commands.entity(entity).remove::<EnableLiveAnalysis>();
    }

    for entity in disable_query.iter() {
        analysis.disable_live();
        data.is_live = false;
        commands.entity(entity).remove::<DisableLiveAnalysis>();
        bevy_log::info!("Live analysis disabled");
    }
}

pub fn live_analysis_sync_system(
    analysis: Option<Res<AnalysisRes>>,
    mut data: ResMut<LiveAnalysisData>,
) {
    if !data.is_live {
        return;
    }
    let Some(analysis) = analysis else { return };

    data.pitch = analysis.live_pitch();
    data.transients = analysis.live_transients();
    data.waveform = analysis.live_waveform();
    data.spectrum = analysis.live_spectrum();
}

/// Bevy plugin: live analysis enable/disable + per-frame pull.
pub struct TuttiAnalysisPlugin;

impl Plugin for TuttiAnalysisPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<EnableLiveAnalysis>()
            .register_type::<DisableLiveAnalysis>();
        app.init_resource::<LiveAnalysisData>().add_systems(
            Update,
            (live_analysis_control_system, live_analysis_sync_system),
        );
    }
}
