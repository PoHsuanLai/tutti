//! The transport state machine: which motion an event actually produces.
//!
//! [`MotionState`] is the published vocabulary; `TransportFsm` is the private
//! machine that decides transitions, and `TransitionResult` is what it hands
//! back for [`MotionFsm`](super::MotionFsm) to publish.

use super::motion::{FadeOut, MotionEvent, Then};
use crate::params::Beat;
use crate::Samples;

/// What the transport is doing.
///
/// The four settled states are what a UI shows; the two `Declick*` states are
/// transient — audio is still sounding while its gain ramps to zero, and the
/// fade's completion is what performs the stop or the jump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum MotionState {
    /// Not moving. The default, and the safe reading of a corrupt mirror byte.
    #[default]
    Stopped,
    /// Playing forward at the session tempo.
    Rolling,
    /// Scrubbing forward.
    FastForward,
    /// Scrubbing backward.
    Rewind,
    /// Fading out, and stopping when the fade reaches zero.
    DeclickToStop,
    /// Fading out, and jumping to a parked target when the fade reaches zero.
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
/// Total rather than fallible: the only writer is
/// [`MotionFsm`](super::MotionFsm), which encodes
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
    /// Stop at the position the fade ended on.
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
    /// Fade out over `frames`, then perform `on_complete`.
    DeclickStarted {
        motion: MotionState,
        frames: Samples,
        on_complete: DeclickOutcome,
    },
}

/// 10 ms at 48 kHz — long enough to kill a click, short enough not to read
/// as a fade.
pub(crate) const DEFAULT_DECLICK_FRAMES: Samples = Samples(480);

/// The machine itself. Audio-thread-only — [`MotionFsm`](super::MotionFsm)
/// keeps it behind an `AudioThreadCell` and publishes its results.
pub(crate) struct TransportFsm {
    motion: MotionState,
    /// The motion to restore when a scrub ends. Never a declick state.
    prev_motion: MotionState,
    /// What the in-flight fade completes into. `None` when no fade is armed.
    pending: Option<DeclickOutcome>,
    declick_frames: Samples,
}

impl TransportFsm {
    pub fn new() -> Self {
        Self {
            motion: MotionState::Stopped,
            prev_motion: MotionState::Stopped,
            pending: None,
            declick_frames: DEFAULT_DECLICK_FRAMES,
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
    /// A fade interrupted by a scrub is abandoned, and the anchor becomes what
    /// the fade was heading *towards*.
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
            frames: self.declick_frames,
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

    /// Apply `event`, returning what changed or `None` if the FSM refused it.
    ///
    /// Every path that returns `Some` assigns `self.motion` first — that is
    /// what keeps the published mirror and the machine from diverging, and it
    /// is why all the arms route through `settle` / `locate_now` /
    /// `start_declick` / `retarget_declick` rather than assigning inline.
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

            MotionEvent::Locate {
                beat: pos,
                fade,
                then,
            } => {
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
                // Not scrubbing — nothing to end. Settling on `prev_motion`
                // here would clobber the motion with a stale anchor and
                // silently stop playback.
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
        let result = fsm.transition(MotionEvent::locate(Beat(8.0)));
        assert!(matches!(
            result,
            Some(TransitionResult::Located {
                motion: MotionState::Stopped,
                ..
            })
        ));

        let result = fsm.transition(MotionEvent::locate_and_play(Beat(4.0)));
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
                frames: DEFAULT_DECLICK_FRAMES,
                on_complete: DeclickOutcome::Stop,
            })
        ));

        // Locate with declick, from rolling
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);
        let result = fsm.transition(MotionEvent::locate(Beat(4.0)));
        assert!(matches!(
            result,
            Some(TransitionResult::DeclickStarted {
                motion: MotionState::DeclickToLocate,
                on_complete: DeclickOutcome::Locate { .. },
                ..
            })
        ));
    }

    /// The FSM and its published mirror must never disagree.
    ///
    /// A transition that reports a motion without storing it leaves later
    /// events dispatching against the wrong state: both stop arms fall into
    /// their `None` case and the transport becomes unstoppable. Every path that
    /// returns `Some` goes through `settle`/`locate_now`/`start_declick`, all
    /// of which assign `self.motion`; this pins that invariant exhaustively
    /// over every (state, event) pair.
    #[test]
    fn every_accepted_transition_settles_the_fsm() {
        let events = [
            MotionEvent::Play,
            MotionEvent::stop(),
            MotionEvent::stop_now(),
            MotionEvent::locate(Beat(16.0)),
            MotionEvent::locate_and_play(Beat(16.0)),
            MotionEvent::stop_and_return(Beat(0.0)),
            MotionEvent::Locate {
                beat: Beat(2.0),
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

    /// Changing scrub direction must not overwrite the anchor with the *other*
    /// scrub state — `EndScrub` would then restore fast-forward instead of
    /// whatever was playing before the scrub began.
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

    /// `prev_motion` must never capture a declick state. Restoring one with the
    /// fade already cleared makes `Engine::apply_declick` early-return, so
    /// `complete_declick` never fires and the FSM hangs in "stopping" forever.
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

    /// `EndScrub` when not scrubbing must change nothing. Settling on the stale
    /// anchor instead would silently stop playback.
    #[test]
    fn end_scrub_while_not_scrubbing_is_a_no_op() {
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);

        assert!(fsm.transition(MotionEvent::EndScrub).is_none());
        assert_eq!(fsm.motion, MotionState::Rolling, "playback must continue");
    }

    /// A discarded jump must not survive. `settle` clears `pending`, so
    /// stopping during a locate-fade drops the fade's parked outcome rather
    /// than letting it resurface on the next completed fade.
    #[test]
    fn stopping_discards_a_pending_locate() {
        let mut fsm = TransportFsm::new();
        fsm.transition(MotionEvent::Play);
        fsm.transition(MotionEvent::stop_and_return(Beat(8.0)));
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
        fsm.transition(MotionEvent::stop_and_return(Beat(0.0)));
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

        let result = fsm.transition(MotionEvent::locate(Beat(12.0)));
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
