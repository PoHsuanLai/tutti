//! Live analysis (pitch, transients, waveform, spectrum) mirror as a Bevy resource.

use std::sync::Arc;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;

use crate::graph::engine_ready;
use crate::resources::AnalysisRes;

/// Fire-and-forget request to enable live analysis.
///
/// Read by `live_analysis_control_system`, calls `engine.enable_live_analysis()`.
#[derive(Message, Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnableLiveAnalysis;

/// Fire-and-forget request to disable live analysis.
///
/// Read by `live_analysis_control_system`, calls `engine.disable_live_analysis()`.
#[derive(Message, Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DisableLiveAnalysis;

/// Live analysis state synced from Tutti via lock-free ArcSwap reads.
///
/// Fields are `Arc` pointers -- cheap to clone for UI consumption.
#[derive(Resource)]
pub struct LiveAnalysisData {
    pub pitch: Arc<tutti_analysis::PitchResult>,
    pub transients: Arc<Vec<tutti_analysis::Transient>>,
    pub waveform: Arc<tutti_analysis::WaveformSummary>,
    pub spectrum: Arc<tutti_analysis::SpectrumResult>,
    pub is_live: bool,
}

impl Default for LiveAnalysisData {
    fn default() -> Self {
        Self {
            pitch: Arc::new(tutti_analysis::PitchResult::default()),
            transients: Arc::new(Vec::new()),
            waveform: Arc::new(tutti_analysis::WaveformSummary::new(512)),
            spectrum: Arc::new(tutti_analysis::SpectrumResult::default()),
            is_live: false,
        }
    }
}

pub fn live_analysis_control_system(
    analysis: Res<AnalysisRes>,
    mut data: ResMut<LiveAnalysisData>,
    mut enable: MessageReader<EnableLiveAnalysis>,
    mut disable: MessageReader<DisableLiveAnalysis>,
) {
    for _ in enable.read() {
        if analysis.enable_live() {
            data.is_live = true;
            bevy_log::info!("Live analysis enabled");
        } else {
            bevy_log::warn!(
                "EnableLiveAnalysis: AnalysisHandle has no metering manager — \
                 analysis thread cannot start"
            );
        }
    }

    for _ in disable.read() {
        analysis.disable_live();
        data.is_live = false;
        bevy_log::info!("Live analysis disabled");
    }
}

pub fn live_analysis_sync_system(
    analysis: Res<AnalysisRes>,
    mut data: ResMut<LiveAnalysisData>,
) {
    if !data.is_live {
        return;
    }

    data.pitch = analysis.live_pitch();
    data.transients = analysis.live_transients();
    data.waveform = analysis.live_waveform();
    data.spectrum = analysis.live_spectrum();
}

/// Bevy plugin: live analysis enable/disable + per-frame pull.
pub struct TuttiAnalysisPlugin;

impl Plugin for TuttiAnalysisPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<EnableLiveAnalysis>()
            .add_message::<DisableLiveAnalysis>();
        app.init_resource::<LiveAnalysisData>().add_systems(
            Update,
            (live_analysis_control_system, live_analysis_sync_system).run_if(engine_ready),
        );
    }
}
