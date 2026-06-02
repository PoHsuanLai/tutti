//! Transport integration for butler thread.
//!
//! Bridges TransportManager events to butler commands for synchronized playback.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use dashmap::DashMap;
use smol::channel::Sender;
use tutti_core::{MotionState, SyncSource, SyncStatus, TransportManager};

use super::command::ButlerCommand;
use super::plan::ChannelPlan;

/// Slaved mode buffer margin multiplier.
/// When synced to external source, use larger buffers to handle jitter.
const SLAVED_BUFFER_MARGIN: f64 = 1.5;

/// Transport bridge that syncs butler state with TransportManager.
///
/// Runs a polling loop that detects transport state changes and
/// translates them to butler commands for ALL active streaming channels.
pub struct TransportBridge {
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TransportBridge {
    /// Create and start a transport bridge.
    ///
    /// The bridge polls the transport manager and sends commands to the butler.
    /// Polling interval is 5ms for responsive transport sync.
    ///
    /// Commands (seek, loop) are broadcast to all active streaming channels.
    pub(crate) fn new(
        transport: Arc<TransportManager>,
        butler_tx: Sender<ButlerCommand>,
        plans: Arc<DashMap<usize, ChannelPlan>>,
        sample_rate: f64,
    ) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let thread = thread::spawn(move || {
            run_bridge(transport, butler_tx, plans, sample_rate, running_clone);
        });

        Self {
            running,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for TransportBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bridge main loop.
fn run_bridge(
    transport: Arc<TransportManager>,
    butler_tx: Sender<ButlerCommand>,
    plans: Arc<DashMap<usize, ChannelPlan>>,
    sample_rate: f64,
    running: Arc<AtomicBool>,
) {
    let mut last_motion = MotionState::Stopped;
    let mut last_beat = 0.0f64;
    let mut last_loop_enabled = false;
    let mut last_loop_range: Option<(f64, f64)> = None;
    let mut last_sync_source = SyncSource::Internal;
    let mut last_sync_locked = false;

    while running.load(Ordering::Acquire) {
        let sync_snap = transport.sync_snapshot();
        let sync_source_changed = sync_snap.source != last_sync_source;
        let sync_lock_changed = (sync_snap.status == SyncStatus::Locked) != last_sync_locked;

        if sync_source_changed || sync_lock_changed {
            let is_slaved = sync_snap.source != SyncSource::Internal
                && sync_snap.status == SyncStatus::Locked
                && sync_snap.following;

            let buffer_margin = if is_slaved { SLAVED_BUFFER_MARGIN } else { 1.0 };

            let _ = butler_tx.try_send(ButlerCommand::SetBufferMargin {
                margin: buffer_margin,
            });

            if is_slaved && sync_snap.external_position > 0.0 {
                let external_samples = (sync_snap.external_position * sample_rate * 60.0
                    / sync_snap.external_tempo) as u64;

                for entry in plans.iter() {
                    let channel_index = *entry.key();
                    if entry.value().link.is_some() {
                        let _ = butler_tx.try_send(ButlerCommand::SeekStream {
                            channel_index,
                            position_samples: external_samples,
                        });
                    }
                }
            }

            last_sync_source = sync_snap.source;
            last_sync_locked = sync_snap.status == SyncStatus::Locked;
        }

        let motion = transport.motion_state();
        if motion != last_motion {
            match motion {
                MotionState::Rolling => {
                    let _ = butler_tx.try_send(ButlerCommand::Run);
                }
                MotionState::Stopped | MotionState::DeclickToStop => {
                    let _ = butler_tx.try_send(ButlerCommand::Pause);
                }
                _ => {}
            }
            last_motion = motion;
        }

        let current_beat = transport.get_current_beat();
        let beat_delta = (current_beat - last_beat).abs();

        if beat_delta > 0.5 && motion != MotionState::Rolling {
            let position_samples = transport.beats_to_samples(current_beat);

            for entry in plans.iter() {
                let channel_index = *entry.key();
                if entry.value().link.is_some() {
                    let _ = butler_tx.try_send(ButlerCommand::SeekStream {
                        channel_index,
                        position_samples,
                    });
                }
            }
        }
        last_beat = current_beat;

        let loop_enabled = transport.is_loop_enabled();
        let loop_range = transport.get_loop_range();

        if loop_enabled != last_loop_enabled || loop_range != last_loop_range {
            for entry in plans.iter() {
                let channel_index = *entry.key();
                if entry.value().link.is_some() {
                    if loop_enabled {
                        if let Some((start, end)) = loop_range {
                            let start_samples = transport.beats_to_samples(start);
                            let end_samples = transport.beats_to_samples(end);
                            let _ = butler_tx.try_send(ButlerCommand::SetLoopRange {
                                channel_index,
                                start_samples,
                                end_samples,
                                crossfade_samples: 64,
                            });
                        }
                    } else {
                        let _ = butler_tx.try_send(ButlerCommand::ClearLoopRange { channel_index });
                    }
                }
            }
            last_loop_enabled = loop_enabled;
            last_loop_range = loop_range;
        }

        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::command::RegionId;
    use crate::butler::prefetch::RegionBuffer;
    use smol::channel::unbounded;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Helper to drain all commands from channel
    fn drain_commands(rx: &smol::channel::Receiver<ButlerCommand>) -> Vec<ButlerCommand> {
        let mut cmds = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            cmds.push(cmd);
        }
        cmds
    }

    /// Wait for bridge to process and collect commands
    fn wait_and_drain(
        rx: &smol::channel::Receiver<ButlerCommand>,
        wait_ms: u64,
    ) -> Vec<ButlerCommand> {
        thread::sleep(Duration::from_millis(wait_ms));
        drain_commands(rx)
    }

    /// Create stream states with actual streaming channels
    fn create_streaming_channels(count: usize) -> Arc<DashMap<usize, ChannelPlan>> {
        let plans = Arc::new(DashMap::new());

        for i in 0..count {
            let region_id = RegionId(i as u64 + 1);
            let (_producer, consumer) =
                RegionBuffer::with_capacity(region_id, PathBuf::from("test.wav"), 4096);

            let mut state = ChannelPlan::default();
            state.start_streaming(Arc::new(parking_lot::Mutex::new(consumer)));
            plans.insert(i, state);
        }

        plans
    }

    #[test]
    fn test_transport_bridge_creation() {
        let transport = Arc::new(TransportManager::default());
        let samples = transport.beats_to_samples(1.0);
        assert!(samples > 0);
    }

    #[test]
    fn test_bridge_start_and_stop() {
        let transport = Arc::new(TransportManager::default());
        let (tx, _rx) = unbounded();
        let plans = Arc::new(DashMap::new());

        let mut bridge = TransportBridge::new(transport, tx, plans, 44100.0);

        // Bridge should be running
        assert!(bridge.running.load(Ordering::Acquire));

        // Stop it
        bridge.stop();

        // Should no longer be running
        assert!(!bridge.running.load(Ordering::Acquire));
        assert!(bridge.thread.is_none());
    }

    #[test]
    fn test_bridge_drop_stops_thread() {
        let transport = Arc::new(TransportManager::default());
        let (tx, _rx) = unbounded();
        let plans = Arc::new(DashMap::new());

        let running = {
            let bridge = TransportBridge::new(transport, tx, plans, 44100.0);
            bridge.running.clone()
        };
        // Bridge dropped here

        // Give thread time to stop
        thread::sleep(Duration::from_millis(20));
        assert!(!running.load(Ordering::Acquire));
    }

    #[test]
    fn test_motion_state_rolling_sends_run() {
        let transport = Arc::new(TransportManager::default());
        let (tx, rx) = unbounded();
        let plans = Arc::new(DashMap::new());

        let mut bridge = TransportBridge::new(transport.clone(), tx, plans, 44100.0);

        // Wait for initial state to settle
        thread::sleep(Duration::from_millis(15));
        drain_commands(&rx);

        // Start playback - need to process commands to update motion_state
        transport.play();
        transport.process_commands(); // Actually update state

        // Wait for bridge to detect and send command
        let cmds = wait_and_drain(&rx, 20);

        bridge.stop();

        // Should have sent Run command
        let has_run = cmds.iter().any(|c| matches!(c, ButlerCommand::Run));
        assert!(has_run, "Expected Run command, got: {:?}", cmds);
    }

    #[test]
    fn test_motion_state_stopped_sends_pause() {
        let transport = Arc::new(TransportManager::default());
        let (tx, rx) = unbounded();
        let plans = Arc::new(DashMap::new());

        let mut bridge = TransportBridge::new(transport.clone(), tx, plans, 44100.0);

        // Start playing first - need to process commands to update motion_state
        transport.play();
        transport.process_commands();
        thread::sleep(Duration::from_millis(15));
        drain_commands(&rx);

        // Now stop - need to process commands
        transport.stop();
        transport.process_commands();

        let cmds = wait_and_drain(&rx, 20);

        bridge.stop();

        // Should have sent Pause command
        let has_pause = cmds.iter().any(|c| matches!(c, ButlerCommand::Pause));
        assert!(has_pause, "Expected Pause command, got: {:?}", cmds);
    }

    #[test]
    fn test_beat_jump_sends_seek_when_stopped() {
        let transport = Arc::new(TransportManager::default());
        let (tx, rx) = unbounded();
        let plans = create_streaming_channels(1);

        let mut bridge = TransportBridge::new(transport.clone(), tx, plans, 44100.0);

        // Wait for initial state
        thread::sleep(Duration::from_millis(15));
        drain_commands(&rx);

        // Locate to a different beat while stopped (>0.5 beat delta)
        transport.locate(4.0);
        transport.process_commands(); // Update current_beat

        let cmds = wait_and_drain(&rx, 20);

        bridge.stop();

        // Should have sent SeekStream for channel 0
        let has_seek = cmds.iter().any(|c| {
            matches!(
                c,
                ButlerCommand::SeekStream {
                    channel_index: 0,
                    ..
                }
            )
        });
        assert!(has_seek, "Expected SeekStream command, got: {:?}", cmds);
    }

    #[test]
    fn test_loop_enabled_sends_set_loop_range() {
        let transport = Arc::new(TransportManager::default());
        let (tx, rx) = unbounded();
        let plans = create_streaming_channels(1);

        let mut bridge = TransportBridge::new(transport.clone(), tx, plans, 44100.0);

        // Wait for initial state
        thread::sleep(Duration::from_millis(15));
        drain_commands(&rx);

        // Enable loop with range
        transport.set_loop_range(1.0, 5.0);
        transport.set_loop_enabled(true);

        let cmds = wait_and_drain(&rx, 20);

        bridge.stop();

        // Should have sent SetLoopRange for channel 0
        let has_loop = cmds.iter().any(|c| {
            matches!(
                c,
                ButlerCommand::SetLoopRange {
                    channel_index: 0,
                    ..
                }
            )
        });
        assert!(has_loop, "Expected SetLoopRange command, got: {:?}", cmds);
    }

    #[test]
    fn test_loop_disabled_sends_clear_loop_range() {
        let transport = Arc::new(TransportManager::default());
        let (tx, rx) = unbounded();
        let plans = create_streaming_channels(1);

        let mut bridge = TransportBridge::new(transport.clone(), tx, plans, 44100.0);

        // Enable loop first
        transport.set_loop_range(1.0, 5.0);
        transport.set_loop_enabled(true);
        thread::sleep(Duration::from_millis(15));
        drain_commands(&rx);

        // Now disable loop
        transport.set_loop_enabled(false);

        let cmds = wait_and_drain(&rx, 20);

        bridge.stop();

        // Should have sent ClearLoopRange for channel 0
        let has_clear = cmds
            .iter()
            .any(|c| matches!(c, ButlerCommand::ClearLoopRange { channel_index: 0 }));
        assert!(
            has_clear,
            "Expected ClearLoopRange command, got: {:?}",
            cmds
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn test_slaved_buffer_margin_constant() {
        // Verify the constant is reasonable for external sync jitter
        assert!(SLAVED_BUFFER_MARGIN > 1.0);
        assert!(SLAVED_BUFFER_MARGIN < 3.0);
    }

    #[test]
    fn test_multiple_cycles_no_duplicate_commands() {
        let transport = Arc::new(TransportManager::default());
        let (tx, rx) = unbounded();
        let plans = Arc::new(DashMap::new());

        let mut bridge = TransportBridge::new(transport.clone(), tx, plans, 44100.0);

        // Wait multiple poll cycles
        thread::sleep(Duration::from_millis(50));
        let _initial_cmds = drain_commands(&rx);

        // Wait more cycles with no state changes
        thread::sleep(Duration::from_millis(50));
        let later_cmds = drain_commands(&rx);

        bridge.stop();

        // Should not receive duplicate commands when state hasn't changed
        // (After initial state is established, no new commands without changes)
        assert!(
            later_cmds.is_empty(),
            "Should not send commands without state changes, got: {:?}",
            later_cmds
        );
    }

    #[test]
    fn test_bridge_handles_rapid_state_changes() {
        let transport = Arc::new(TransportManager::default());
        let (tx, rx) = unbounded();
        let plans = Arc::new(DashMap::new());

        let mut bridge = TransportBridge::new(transport.clone(), tx, plans, 44100.0);

        // Rapid play/stop toggles - need process_commands() for state updates
        for _ in 0..5 {
            transport.play();
            transport.process_commands();
            thread::sleep(Duration::from_millis(10)); // Give bridge time to poll
            transport.stop();
            transport.process_commands();
            thread::sleep(Duration::from_millis(10));
        }

        thread::sleep(Duration::from_millis(20));
        let cmds = drain_commands(&rx);

        bridge.stop();

        // Should have multiple Run and Pause commands
        let run_count = cmds
            .iter()
            .filter(|c| matches!(c, ButlerCommand::Run))
            .count();
        let pause_count = cmds
            .iter()
            .filter(|c| matches!(c, ButlerCommand::Pause))
            .count();

        // Should have captured at least some of the state changes
        assert!(run_count > 0, "Should have some Run commands");
        assert!(pause_count > 0, "Should have some Pause commands");
    }
}
