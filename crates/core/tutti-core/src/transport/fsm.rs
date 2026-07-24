//! Transport state machine.

use super::motion::MotionEvent;
use super::position::MusicalPosition;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum MotionState {
    #[default]
    Stopped,
    Rolling,
    FastForward,
    Rewind,
    DeclickToStop,
    DeclickToLocate,
}

/// Encode for the published `AtomicU8` mirror. `#[repr(u8)]` makes the cast
/// exact, and the discriminants are the enum's own — not a parallel table that
/// could drift from it.
impl From<MotionState> for u8 {
    #[inline]
    fn from(state: MotionState) -> u8 {
        state as u8
    }
}

/// Decode from the published mirror.
///
/// Total rather than fallible: the only writer is [`MotionFsm`], which encodes
/// via the `From` above, so an out-of-range byte cannot occur through the
/// public API. A torn or corrupt read degrades to `Stopped` — the safe
/// interpretation for a transport — rather than panicking on the audio thread.
impl From<u8> for MotionState {
    #[inline]
    fn from(val: u8) -> Self {
        match val {
            1 => Self::Rolling,
            2 => Self::FastForward,
            3 => Self::Rewind,
            4 => Self::DeclickToStop,
            5 => Self::DeclickToLocate,
            _ => Self::Stopped,
        }
    }
}

impl core::fmt::Display for MotionState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Stopped => "stopped",
            Self::Rolling => "rolling",
            Self::FastForward => "fast-forward",
            Self::Rewind => "rewind",
            Self::DeclickToStop => "stopping",
            Self::DeclickToLocate => "seeking",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LocateState {
    #[default]
    Idle,
    LocateAndStop,
    LocateAndRoll,
}

/// What a transition changed. Loop state is deliberately absent: the FSM
/// holds none, so routing loop writes through it was a round-trip that
/// computed nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TransitionResult {
    MotionChanged(MotionState),
    Locating(MusicalPosition),
    DeclickStarted(MotionState),
}

pub(crate) const DEFAULT_DECLICK_SAMPLES: usize = 480;

pub(crate) struct TransportFsm {
    motion: MotionState,
    locate: LocateState,
    pending_locate: Option<MusicalPosition>,
    prev_motion: MotionState,
    declick_samples: usize,
}

impl TransportFsm {
    pub fn new() -> Self {
        Self {
            motion: MotionState::Stopped,
            locate: LocateState::Idle,
            pending_locate: None,
            prev_motion: MotionState::Stopped,
            declick_samples: DEFAULT_DECLICK_SAMPLES,
        }
    }

    pub fn locate_state(&self) -> LocateState {
        self.locate
    }

    pub fn pending_locate(&self) -> Option<MusicalPosition> {
        self.pending_locate
    }

    pub fn declick_samples(&self) -> usize {
        self.declick_samples
    }

    pub fn transition(&mut self, event: MotionEvent) -> Option<TransitionResult> {
        use MotionEvent::*;

        match event {
            Play => match self.motion {
                MotionState::Stopped
                | MotionState::FastForward
                | MotionState::Rewind
                | MotionState::DeclickToStop => {
                    self.motion = MotionState::Rolling;
                    self.locate = LocateState::Idle;
                    Some(TransitionResult::MotionChanged(MotionState::Rolling))
                }
                MotionState::Rolling | MotionState::DeclickToLocate => None,
            },

            StopNow => match self.motion {
                MotionState::Rolling | MotionState::FastForward | MotionState::Rewind => {
                    self.motion = MotionState::Stopped;
                    self.locate = LocateState::Idle;
                    Some(TransitionResult::MotionChanged(MotionState::Stopped))
                }
                MotionState::DeclickToStop | MotionState::DeclickToLocate => {
                    self.motion = MotionState::Stopped;
                    Some(TransitionResult::MotionChanged(MotionState::Stopped))
                }
                MotionState::Stopped => None,
            },

            Stop => match self.motion {
                MotionState::Rolling => {
                    self.motion = MotionState::DeclickToStop;
                    self.locate = LocateState::Idle;
                    Some(TransitionResult::DeclickStarted(MotionState::DeclickToStop))
                }
                MotionState::FastForward | MotionState::Rewind => {
                    self.motion = MotionState::Stopped;
                    Some(TransitionResult::MotionChanged(MotionState::Stopped))
                }
                _ => None,
            },

            Locate(beats) => {
                let pos = MusicalPosition::from_beats(beats);
                self.pending_locate = Some(pos);
                self.locate = LocateState::LocateAndStop;
                Some(TransitionResult::Locating(pos))
            }

            LocateWithDeclick(beats) => {
                let pos = MusicalPosition::from_beats(beats);
                self.pending_locate = Some(pos);
                self.locate = LocateState::LocateAndStop;
                if self.motion == MotionState::Rolling {
                    // Fade out first; the locate lands when the fade completes.
                    self.motion = MotionState::DeclickToLocate;
                    Some(TransitionResult::DeclickStarted(
                        MotionState::DeclickToLocate,
                    ))
                } else {
                    Some(TransitionResult::Locating(pos))
                }
            }

            LocateAndPlay(beats) => {
                let pos = MusicalPosition::from_beats(beats);
                self.pending_locate = Some(pos);
                self.locate = LocateState::LocateAndRoll;
                Some(TransitionResult::Locating(pos))
            }

            FastForward => {
                self.prev_motion = self.motion;
                self.motion = MotionState::FastForward;
                Some(TransitionResult::MotionChanged(MotionState::FastForward))
            }

            Rewind => {
                self.prev_motion = self.motion;
                self.motion = MotionState::Rewind;
                Some(TransitionResult::MotionChanged(MotionState::Rewind))
            }

            EndScrub => {
                self.motion = self.prev_motion;
                Some(TransitionResult::MotionChanged(self.motion))
            }
        }
    }
}

impl Default for TransportFsm {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_play_stop_transitions() {
        let mut fsm = TransportFsm::new();

        // Play from stopped
        let result = fsm.transition(MotionEvent::Play);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::Rolling))
        ));

        // Stop while rolling
        let result = fsm.transition(MotionEvent::StopNow);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::Stopped))
        ));

        // Play again (idempotent)
        fsm.transition(MotionEvent::Play);
        let result = fsm.transition(MotionEvent::Play);
        assert!(result.is_none());
    }

    #[test]
    fn test_locate_events() {
        let mut fsm = TransportFsm::new();

        let target = 8.0;
        let result = fsm.transition(MotionEvent::Locate(target));
        assert!(matches!(result, Some(TransitionResult::Locating(_))));

        let target = 4.0;
        let result = fsm.transition(MotionEvent::LocateAndPlay(target));
        assert!(matches!(result, Some(TransitionResult::Locating(_))));
    }

    #[test]
    fn test_scrub_events() {
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);

        // Fast forward
        let result = fsm.transition(MotionEvent::FastForward);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::FastForward))
        ));

        // End scrub - should return to rolling
        let result = fsm.transition(MotionEvent::EndScrub);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::Rolling))
        ));

        // Rewind
        let result = fsm.transition(MotionEvent::Rewind);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::Rewind))
        ));

        // End scrub again
        let result = fsm.transition(MotionEvent::EndScrub);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::Rolling))
        ));
    }

    #[test]
    fn test_declick_events() {
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);

        // Stop with declick
        let result = fsm.transition(MotionEvent::Stop);
        assert!(matches!(
            result,
            Some(TransitionResult::DeclickStarted(MotionState::DeclickToStop))
        ));

        // Locate with declick
        fsm.transition(MotionEvent::Play);
        let target = 4.0;
        let result = fsm.transition(MotionEvent::LocateWithDeclick(target));
        assert!(matches!(
            result,
            Some(TransitionResult::DeclickStarted(
                MotionState::DeclickToLocate
            ))
        ));
    }

    #[test]
    fn motion_state_u8_roundtrip_is_exact() {
        for state in [
            MotionState::Stopped,
            MotionState::Rolling,
            MotionState::FastForward,
            MotionState::Rewind,
            MotionState::DeclickToStop,
            MotionState::DeclickToLocate,
        ] {
            let byte: u8 = state.into();
            assert_eq!(
                MotionState::from(byte),
                state,
                "{state} did not survive the atomic round-trip"
            );
        }
    }

    #[test]
    fn motion_state_decodes_unknown_bytes_as_stopped() {
        // Not reachable through the public API — the only writer encodes via
        // `From` — but a transport must degrade to "not moving", never panic.
        assert_eq!(MotionState::from(6), MotionState::Stopped);
        assert_eq!(MotionState::from(u8::MAX), MotionState::Stopped);
    }
}
