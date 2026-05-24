use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::TransportRes;

/// Transport state synced from Tutti every frame via lock-free atomics.
#[derive(Resource, Debug, Clone, Reflect)]
#[reflect(Resource, Default, Clone)]
pub struct TransportState {
    pub tempo: f64,
    pub beat: f64,
    pub is_playing: bool,
    pub is_paused: bool,
    pub is_recording: bool,
    pub is_looping: bool,
    pub loop_start: f64,
    pub loop_end: f64,
    pub time_sig_numerator: u8,
    pub time_sig_denominator: u8,
}

impl Default for TransportState {
    fn default() -> Self {
        Self {
            tempo: 120.0,
            beat: 0.0,
            is_playing: false,
            is_paused: true,
            is_recording: false,
            is_looping: false,
            loop_start: 0.0,
            loop_end: 16.0,
            time_sig_numerator: 4,
            time_sig_denominator: 4,
        }
    }
}

impl TransportState {
    pub fn beat(&self) -> f64 {
        self.beat
    }

    pub fn tempo(&self) -> f64 {
        self.tempo
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing
    }

    pub fn is_looping(&self) -> bool {
        self.is_looping
    }

    pub fn loop_range(&self) -> (f64, f64) {
        (self.loop_start, self.loop_end)
    }
}

pub fn transport_sync_system(
    transport: Option<Res<TransportRes>>,
    mut state: ResMut<TransportState>,
) {
    let Some(transport) = transport else { return };
    state.beat = transport.current_beat();
    state.is_playing = transport.is_playing();
    state.is_recording = transport.is_recording();
    state.tempo = transport.get_tempo().get();
    state.is_looping = transport.is_loop_enabled();
    if let Some((start, end)) = transport.get_loop_range() {
        state.loop_start = start;
        state.loop_end = end;
    }
}
