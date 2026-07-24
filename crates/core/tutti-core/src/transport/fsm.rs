//! Transport state machine.

use super::motion::{FadeOut, MotionEvent, Then};
use crate::params::Beat;

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

/// What a completed declick fade performs when it reaches zero.
///
/// Decided at transition time and parked in the FSM, so completion is a lookup
/// rather than a re-derivation. Loop state is deliberately absent: the FSM
/// holds none, so routing loop writes through it was a round-trip that
/// computed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) enum DeclickOutcome {
    /// Stop where we are.
    #[default]
    Stop,
    /// Jump to `pos`, landing in `motion`.
    Locate { pos: Beat, motion: MotionState },
}

/// What a transition changed.
///
/// Self-describing: [`MotionFsm::publish`](super::MotionFsm) applies it without
/// reading the FSM back, which is what keeps the audio thread down to one cell
/// borrow per event. Every variant carries the motion the FSM *already stored*,
/// never something the caller re-derives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TransitionResult {
    /// Motion changed immediately. Cancels any fade in progress — unless the
    /// new motion is itself a declick state, which means a fade was retargeted
    /// in place and its ramp must keep counting.
    MotionChanged(MotionState),
    /// Jump now, landing in `motion`.
    Located { pos: Beat, motion: MotionState },
    /// Fade out over `samples`, then perform `on_complete`.
    DeclickStarted {
        motion: MotionState,
        samples: u32,
        on_complete: DeclickOutcome,
    },
}

pub(crate) const DEFAULT_DECLICK_SAMPLES: u32 = 480;

pub(crate) struct TransportFsm {
    motion: MotionState,
    prev_motion: MotionState,
    /// What the in-flight fade completes into. `None` when no fade is armed.
    pending: Option<DeclickOutcome>,
    declick_samples: u32,
}

impl TransportFsm {
    pub fn new() -> Self {
        Self {
            motion: MotionState::Stopped,
            prev_motion: MotionState::Stopped,
            pending: None,
            declick_samples: DEFAULT_DECLICK_SAMPLES,
        }
    }

    /// Take the outcome of a completed fade. One-shot: a second call after the
    /// same fade yields `None`.
    pub fn take_declick_outcome(&mut self) -> Option<DeclickOutcome> {
        self.pending.take()
    }

    /// The motion to remember for [`MotionEvent::EndScrub`].
    ///
    /// A declick state must never be remembered: restoring `DeclickToStop` with
    /// `remaining == 0` makes `Engine::apply_declick` early-return, so
    /// `complete_declick` never fires and the FSM hangs in that state forever.
    /// A fade interrupted by a scrub is abandoned, and what it was fading
    /// *towards* is what we return to.
    ///
    /// Scrubbing while already scrubbing keeps the pre-scrub anchor, so
    /// fast-forward → rewind → end-scrub returns to what was playing rather
    /// than to fast-forward.
    #[inline]
    fn scrub_anchor(&self) -> MotionState {
        match self.motion {
            MotionState::FastForward | MotionState::Rewind => self.prev_motion,
            MotionState::DeclickToStop | MotionState::DeclickToLocate => MotionState::Stopped,
            settled => settled,
        }
    }

    /// Settle into `motion` immediately, abandoning any armed fade.
    #[inline]
    fn settle(&mut self, motion: MotionState) -> Option<TransitionResult> {
        self.motion = motion;
        self.pending = None;
        Some(TransitionResult::MotionChanged(motion))
    }

    /// Jump now, landing in `motion`.
    #[inline]
    fn locate_now(&mut self, pos: Beat, motion: MotionState) -> Option<TransitionResult> {
        self.motion = motion;
        self.pending = None;
        Some(TransitionResult::Located { pos, motion })
    }

    /// Arm a fade and record what it completes into.
    #[inline]
    fn start_declick(
        &mut self,
        motion: MotionState,
        on_complete: DeclickOutcome,
    ) -> Option<TransitionResult> {
        self.motion = motion;
        self.pending = Some(on_complete);
        Some(TransitionResult::DeclickStarted {
            motion,
            samples: self.declick_samples,
            on_complete,
        })
    }

    /// Retarget an in-flight fade without restarting its ramp.
    ///
    /// `Declick::start` resets `remaining` to full, which mid-fade steps the
    /// gain back to 1.0 — an audible click, precisely what the fade exists to
    /// prevent. So a fade that changes its mind keeps counting and only swaps
    /// what it completes into.
    #[inline]
    fn retarget_declick(
        &mut self,
        motion: MotionState,
        on_complete: DeclickOutcome,
    ) -> Option<TransitionResult> {
        self.motion = motion;
        self.pending = Some(on_complete);
        Some(TransitionResult::MotionChanged(motion))
    }

    /// Whether sound is still coming out, i.e. there is something to fade out
    /// of. Asking for a fade from a stop is not an error — it is satisfied
    /// instantly.
    ///
    /// A declick state counts as audible: a fade is *audio still playing* while
    /// its gain ramps down, which is the entire reason it exists. Treating it as
    /// silent would let a locate mid-fade jump immediately and cut the sound the
    /// fade was protecting.
    #[inline]
    fn is_audible(&self) -> bool {
        !matches!(self.motion, MotionState::Stopped)
    }

    pub fn transition(&mut self, event: MotionEvent) -> Option<TransitionResult> {
        use MotionState as S;

        match event {
            MotionEvent::Play => match self.motion {
                // A Play during a fade cancels it, including a pending jump.
                S::Stopped | S::FastForward | S::Rewind | S::DeclickToStop | S::DeclickToLocate => {
                    self.settle(S::Rolling)
                }
                S::Rolling => None,
            },

            MotionEvent::Stop {
                fade: FadeOut::Immediate,
            } => match self.motion {
                S::Stopped => None,
                _ => self.settle(S::Stopped),
            },

            MotionEvent::Stop {
                fade: FadeOut::Declick,
            } => match self.motion {
                S::Rolling => self.start_declick(S::DeclickToStop, DeclickOutcome::Stop),
                // Scrubbing is not material worth fading, and fading it would
                // just extend the scrub.
                S::FastForward | S::Rewind => self.settle(S::Stopped),
                // Already fading to a stop — let the ramp finish.
                S::DeclickToStop => None,
                // Fading to a locate, now asked to stop: keep the ramp, change
                // only where it lands.
                S::DeclickToLocate => self.retarget_declick(S::DeclickToStop, DeclickOutcome::Stop),
                S::Stopped => None,
            },

            MotionEvent::Locate { beat, fade, then } => {
                let pos = Beat(beat);

                // Resolve `Keep` against the *settled* motion, so a locate
                // during a fade means "whatever the fade was heading for",
                // not "mid-fade".
                let landing = match then {
                    Then::Roll => S::Rolling,
                    Then::Stop => S::Stopped,
                    Then::Keep => match self.motion {
                        S::Rolling | S::FastForward | S::Rewind => S::Rolling,
                        S::Stopped | S::DeclickToStop | S::DeclickToLocate => S::Stopped,
                    },
                };

                if fade == FadeOut::Immediate || !self.is_audible() {
                    return self.locate_now(pos, landing);
                }

                let outcome = DeclickOutcome::Locate {
                    pos,
                    motion: landing,
                };
                // Already fading: retarget rather than re-arm, so the ramp does
                // not jump back to full gain.
                if matches!(self.motion, S::DeclickToStop | S::DeclickToLocate) {
                    self.retarget_declick(S::DeclickToLocate, outcome)
                } else {
                    self.start_declick(S::DeclickToLocate, outcome)
                }
            }

            MotionEvent::FastForward => {
                self.prev_motion = self.scrub_anchor();
                self.settle(S::FastForward)
            }

            MotionEvent::Rewind => {
                self.prev_motion = self.scrub_anchor();
                self.settle(S::Rewind)
            }

            MotionEvent::EndScrub => match self.motion {
                S::FastForward | S::Rewind => self.settle(self.prev_motion),
                // Not scrubbing — nothing to end. Previously this clobbered the
                // motion with a stale anchor, silently stopping playback.
                _ => None,
            },
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
        let result = fsm.transition(MotionEvent::stop_now());
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

        // From a stop there is nothing to fade, so a locate lands immediately.
        let result = fsm.transition(MotionEvent::locate(8.0));
        assert!(matches!(
            result,
            Some(TransitionResult::Located {
                motion: MotionState::Stopped,
                ..
            })
        ));

        let result = fsm.transition(MotionEvent::locate_and_play(4.0));
        assert!(matches!(
            result,
            Some(TransitionResult::Located {
                motion: MotionState::Rolling,
                ..
            })
        ));
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
        let result = fsm.transition(MotionEvent::stop());
        assert!(matches!(
            result,
            Some(TransitionResult::DeclickStarted {
                motion: MotionState::DeclickToStop,
                samples: DEFAULT_DECLICK_SAMPLES,
                on_complete: DeclickOutcome::Stop,
            })
        ));

        // Locate with declick, from rolling
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);
        let result = fsm.transition(MotionEvent::locate(4.0));
        assert!(matches!(
            result,
            Some(TransitionResult::DeclickStarted {
                motion: MotionState::DeclickToLocate,
                on_complete: DeclickOutcome::Locate { .. },
                ..
            })
        ));
    }

    /// D3: `LocateAndPlay` used to return `Locating` without ever assigning
    /// `self.motion`, so the FSM stayed `Stopped` while the published mirror
    /// said `Rolling`. Every later event then dispatched against the wrong
    /// state and both stop arms became no-ops — an unstoppable transport.
    ///
    /// The rewrite makes that unrepresentable: every path that returns `Some`
    /// goes through `settle`/`locate_now`/`start_declick`, all of which assign
    /// `self.motion`. This test pins the invariant directly.
    #[test]
    fn every_accepted_transition_settles_the_fsm() {
        let events = [
            MotionEvent::Play,
            MotionEvent::stop(),
            MotionEvent::stop_now(),
            MotionEvent::locate(16.0),
            MotionEvent::locate_and_play(16.0),
            MotionEvent::stop_and_return(0.0),
            MotionEvent::Locate {
                beat: 2.0,
                fade: FadeOut::Immediate,
                then: Then::Keep,
            },
            MotionEvent::FastForward,
            MotionEvent::Rewind,
            MotionEvent::EndScrub,
        ];
        let states = [
            MotionState::Stopped,
            MotionState::Rolling,
            MotionState::FastForward,
            MotionState::Rewind,
            MotionState::DeclickToStop,
            MotionState::DeclickToLocate,
        ];

        for start in states {
            for event in events {
                let mut fsm = TransportFsm::new();
                fsm.motion = start;

                if let Some(result) = fsm.transition(event) {
                    let published = match result {
                        TransitionResult::MotionChanged(m)
                        | TransitionResult::Located { motion: m, .. }
                        | TransitionResult::DeclickStarted { motion: m, .. } => m,
                    };
                    assert_eq!(
                        fsm.motion, published,
                        "{event:?} from {start:?} published {published:?} but the FSM holds {:?}",
                        fsm.motion
                    );
                }
            }
        }
    }

    /// D5: both scrub arms assigned `prev_motion` unconditionally, so changing
    /// scrub direction overwrote the anchor with the *other* scrub state and
    /// `EndScrub` restored fast-forward instead of what was playing.
    #[test]
    fn scrub_direction_change_keeps_the_pre_scrub_anchor() {
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);
        fsm.transition(MotionEvent::FastForward);
        fsm.transition(MotionEvent::Rewind);

        let result = fsm.transition(MotionEvent::EndScrub);
        assert!(
            matches!(
                result,
                Some(TransitionResult::MotionChanged(MotionState::Rolling))
            ),
            "FF -> REW -> EndScrub must return to Rolling, got {result:?}"
        );
    }

    /// D2: `prev_motion` could capture `DeclickToStop`. `EndScrub` then restored
    /// it with the fade already cleared, so `Engine::apply_declick` early-returns,
    /// `complete_declick` never fires, and the FSM hangs in "stopping" forever.
    #[test]
    fn scrubbing_never_resurrects_an_abandoned_fade() {
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);
        fsm.transition(MotionEvent::stop());
        assert_eq!(fsm.motion, MotionState::DeclickToStop);

        fsm.transition(MotionEvent::FastForward);
        let result = fsm.transition(MotionEvent::EndScrub);

        assert!(
            matches!(
                result,
                Some(TransitionResult::MotionChanged(MotionState::Stopped))
            ),
            "a fade abandoned by a scrub must settle, not be restored: {result:?}"
        );
    }

    /// `EndScrub` when not scrubbing used to clobber the motion with a stale
    /// anchor, silently stopping playback.
    #[test]
    fn end_scrub_while_not_scrubbing_is_a_no_op() {
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);

        assert!(fsm.transition(MotionEvent::EndScrub).is_none());
        assert_eq!(fsm.motion, MotionState::Rolling, "playback must continue");
    }

    /// D4: the locate bookkeeping was never cleared on the arms that returned
    /// `None`, so a discarded jump could survive indefinitely. `settle` now
    /// clears `pending`, so a stop discards the fade's pending outcome.
    #[test]
    fn stopping_discards_a_pending_locate() {
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);
        fsm.transition(MotionEvent::stop_and_return(8.0));
        assert!(fsm.pending.is_some(), "the fade armed a pending locate");

        fsm.transition(MotionEvent::stop_now());
        assert!(
            fsm.take_declick_outcome().is_none(),
            "a discarded locate must not resurface"
        );
    }

    /// Retargeting an in-flight fade must not re-arm the ramp: `Declick::start`
    /// resets `remaining` to full, which mid-fade steps the gain back to 1.0 and
    /// clicks — exactly what the fade exists to prevent. A retarget reports
    /// `MotionChanged` (which `publish` treats as "leave the ramp alone"),
    /// never `DeclickStarted`.
    #[test]
    fn retargeting_a_fade_does_not_restart_the_ramp() {
        // Locate-fade, then asked to stop instead.
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);
        fsm.transition(MotionEvent::stop_and_return(0.0));
        assert_eq!(fsm.motion, MotionState::DeclickToLocate);

        let result = fsm.transition(MotionEvent::stop());
        assert!(
            matches!(
                result,
                Some(TransitionResult::MotionChanged(MotionState::DeclickToStop))
            ),
            "retarget must not re-arm: {result:?}"
        );

        // Stop-fade, then asked to locate instead.
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);
        fsm.transition(MotionEvent::stop());
        assert_eq!(fsm.motion, MotionState::DeclickToStop);

        let result = fsm.transition(MotionEvent::locate(12.0));
        assert!(
            matches!(
                result,
                Some(TransitionResult::MotionChanged(
                    MotionState::DeclickToLocate
                ))
            ),
            "retarget must not re-arm: {result:?}"
        );
    }

    /// A second `Stop` during a stop-fade must let the ramp finish rather than
    /// restarting it.
    #[test]
    fn stop_during_a_stop_fade_is_a_no_op() {
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);
        fsm.transition(MotionEvent::stop());

        assert!(fsm.transition(MotionEvent::stop()).is_none());
        assert_eq!(fsm.motion, MotionState::DeclickToStop);
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
