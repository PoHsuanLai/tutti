//! Transport state machine.

use super::position::{LoopRange, MusicalPosition};

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

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TransportEvent {
    Play,
    Stop,
    StopWithDeclick,
    Locate(MusicalPosition),
    LocateWithDeclick(MusicalPosition),
    LocateAndPlay(MusicalPosition),
    SetLoopEnabled(bool),
    SetLoopRange(LoopRange),
    ClearLoop,
    FastForward,
    Rewind,
    EndScrub,
    Reverse,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TransitionResult {
    MotionChanged(MotionState),
    Locating(MusicalPosition),
    LoopModeChanged(bool),
    DirectionChanged(Direction),
    DeclickStarted(MotionState),
}

pub(crate) const DEFAULT_DECLICK_SAMPLES: usize = 480;

pub(crate) struct TransportFSM {
    motion: MotionState,
    locate: LocateState,
    pending_locate: Option<MusicalPosition>,
    prev_motion: MotionState,
    direction: Direction,
    declick_samples: usize,
}

impl TransportFSM {
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

    pub fn transition(&mut self, event: TransportEvent) -> Option<TransitionResult> {
        use TransportEvent::*;

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

            Stop => match self.motion {
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

            StopWithDeclick => match self.motion {
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

            Locate(pos) => {
                self.pending_locate = Some(pos);
                self.locate = LocateState::LocateAndStop;
                Some(TransitionResult::Locating(pos))
            }

            LocateWithDeclick(pos) => match self.motion {
                MotionState::Rolling => {
                    self.pending_locate = Some(pos);
                    self.locate = LocateState::LocateAndStop;
                    self.motion = MotionState::DeclickToLocate;
                    Some(TransitionResult::DeclickStarted(
                        MotionState::DeclickToLocate,
                    ))
                }
                _ => {
                    self.pending_locate = Some(pos);
                    self.locate = LocateState::LocateAndStop;
                    Some(TransitionResult::Locating(pos))
                }
            },

            LocateAndPlay(pos) => {
                self.pending_locate = Some(pos);
                self.locate = LocateState::LocateAndRoll;
                Some(TransitionResult::Locating(pos))
            }

            SetLoopEnabled(enabled) => Some(TransitionResult::LoopModeChanged(enabled)),

            SetLoopRange(range) => {
                let _ = range; // Loop range is stored in manager atomics
                Some(TransitionResult::LoopModeChanged(true))
            }

            ClearLoop => Some(TransitionResult::LoopModeChanged(false)),

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

impl Default for TransportFSM {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_play_stop_transitions() {
        let mut fsm = TransportFSM::new();

        // Play from stopped
        let result = fsm.transition(TransportEvent::Play);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::Rolling))
        ));

        // Stop while rolling
        let result = fsm.transition(TransportEvent::Stop);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::Stopped))
        ));

        // Play again (idempotent)
        fsm.transition(TransportEvent::Play);
        let result = fsm.transition(TransportEvent::Play);
        assert!(result.is_none());
    }

    #[test]
    fn test_locate_events() {
        let mut fsm = TransportFSM::new();

        let target = MusicalPosition::from_beats(8.0);
        let result = fsm.transition(TransportEvent::Locate(target));
        assert!(matches!(result, Some(TransitionResult::Locating(_))));

        let target = MusicalPosition::from_beats(4.0);
        let result = fsm.transition(TransportEvent::LocateAndPlay(target));
        assert!(matches!(result, Some(TransitionResult::Locating(_))));
    }

    #[test]
    fn test_loop_events() {
        let mut fsm = TransportFSM::new();

        // Set loop range
        let result = fsm.transition(TransportEvent::SetLoopRange(LoopRange::new(0.0, 8.0)));
        assert!(matches!(
            result,
            Some(TransitionResult::LoopModeChanged(true))
        ));

        // Disable
        let result = fsm.transition(TransportEvent::SetLoopEnabled(false));
        assert!(matches!(
            result,
            Some(TransitionResult::LoopModeChanged(false))
        ));

        // Enable
        let result = fsm.transition(TransportEvent::SetLoopEnabled(true));
        assert!(matches!(
            result,
            Some(TransitionResult::LoopModeChanged(true))
        ));

        // Clear loop
        let result = fsm.transition(TransportEvent::ClearLoop);
        assert!(matches!(
            result,
            Some(TransitionResult::LoopModeChanged(false))
        ));
    }

    #[test]
    fn test_scrub_events() {
        let mut fsm = TransportFSM::new();
        fsm.transition(TransportEvent::Play);

        // Fast forward
        let result = fsm.transition(TransportEvent::FastForward);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::FastForward))
        ));

        // End scrub - should return to rolling
        let result = fsm.transition(TransportEvent::EndScrub);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::Rolling))
        ));

        // Rewind
        let result = fsm.transition(TransportEvent::Rewind);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::Rewind))
        ));

        // End scrub again
        let result = fsm.transition(TransportEvent::EndScrub);
        assert!(matches!(
            result,
            Some(TransitionResult::MotionChanged(MotionState::Rolling))
        ));
    }

    #[test]
    fn test_declick_events() {
        let mut fsm = TransportFSM::new();
        fsm.transition(TransportEvent::Play);

        // Stop with declick
        let result = fsm.transition(TransportEvent::StopWithDeclick);
        assert!(matches!(
            result,
            Some(TransitionResult::DeclickStarted(MotionState::DeclickToStop))
        ));

        // Locate with declick
        fsm.transition(TransportEvent::Play);
        let target = MusicalPosition::from_beats(4.0);
        let result = fsm.transition(TransportEvent::LocateWithDeclick(target));
        assert!(matches!(
            result,
            Some(TransitionResult::DeclickStarted(
                MotionState::DeclickToLocate
            ))
        ));
    }

    #[test]
    fn test_reverse_direction() {
        let mut fsm = TransportFSM::new();

        let result = fsm.transition(TransportEvent::Reverse);
        assert!(matches!(
            result,
            Some(TransitionResult::DirectionChanged(Direction::Backwards))
        ));

        let result = fsm.transition(TransportEvent::Reverse);
        assert!(matches!(
            result,
            Some(TransitionResult::DirectionChanged(Direction::Forwards))
        ));
    }
}
