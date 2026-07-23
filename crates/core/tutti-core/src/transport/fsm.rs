//! Transport state machine.

use super::motion::MotionEvent;
use super::position::MusicalPosition;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MotionState {
    #[default]
    Stopped,
    Rolling,
    FastForward,
    Rewind,
    DeclickToStop,
    DeclickToLocate,
}

impl MotionState {
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Rolling => 1,
            Self::FastForward => 2,
            Self::Rewind => 3,
            Self::DeclickToStop => 4,
            Self::DeclickToLocate => 5,
        }
    }

    pub(crate) fn from_u8(val: u8) -> Self {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Forwards,
    Backwards,
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
    DirectionChanged(Direction),
    DeclickStarted(MotionState),
}

pub(crate) const DEFAULT_DECLICK_SAMPLES: usize = 480;

pub(crate) struct TransportFsm {
    motion: MotionState,
    locate: LocateState,
    pending_locate: Option<MusicalPosition>,
    prev_motion: MotionState,
    direction: Direction,
    declick_samples: usize,
}

impl TransportFsm {
    pub fn new() -> Self {
        Self {
            motion: MotionState::Stopped,
            locate: LocateState::Idle,
            pending_locate: None,
            prev_motion: MotionState::Stopped,
            direction: Direction::Forwards,
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

            Reverse => {
                self.direction = match self.direction {
                    Direction::Forwards => Direction::Backwards,
                    Direction::Backwards => Direction::Forwards,
                };
                Some(TransitionResult::DirectionChanged(self.direction))
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
    fn test_reverse_direction() {
        let mut fsm = TransportFsm::new();

        let result = fsm.transition(MotionEvent::Reverse);
        assert!(matches!(
            result,
            Some(TransitionResult::DirectionChanged(Direction::Backwards))
        ));

        let result = fsm.transition(MotionEvent::Reverse);
        assert!(matches!(
            result,
            Some(TransitionResult::DirectionChanged(Direction::Forwards))
        ));
    }
}
