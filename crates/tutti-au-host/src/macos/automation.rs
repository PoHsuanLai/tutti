//! Automation at its frame: the points of a block, staged for the pre-render
//! notify that schedules them into the render in flight.
//!
//! `AudioUnitScheduleParameters` "is used to schedule intra-buffer changes to
//! the value of a parameter ... All of the parameter events must apply to the
//! current (and only apply to the current) audio unit render call, so the
//! events are scheduled as a part of the pre-render notification callback"
//! (`AUComponent.h`). So the block's points cannot be written from `render`
//! before it calls `AudioUnitRender`: they are staged here, and the notify the
//! activation installs ([`schedule_pending`]) hands them to the unit from inside
//! that call. A queue's points are linear breakpoints, so each segment between
//! two points at different offsets is one ramped event (start value, end value,
//! start offset, duration in frames) and a step or a lone point is an immediate
//! event.
//!
//! Both halves run on one thread, one after the other, inside a render; the
//! [`AudioThreadCell`] is the checked form of that, not a lock.

use std::sync::Arc;

use tutti_au_ffi::{ParamEvent, RenderNotification, RenderPhase, RenderUnit, ScheduleAddress};
use tutti_plugin_types::{ParamMapping, ParameterQueue};
use tutti_rt::AudioThreadCell;

use crate::ranges::ParamBounds;

/// Events one block can stage across every parameter: past it a queue is
/// written at the block head instead (the last point), which costs timing and
/// never audio.
const EVENT_ROOM: usize = 4096;
/// Parameters one block can stage.
const RUN_ROOM: usize = 256;

/// One block's staged automation.
pub(crate) struct Stage {
    /// Every staged event, queue after queue.
    events: Vec<ParamEvent>,
    /// `(parameter, end)`: the parameter whose events are `events[start..end]`,
    /// where `start` is the previous run's end.
    runs: Vec<(u32, usize)>,
    /// The value each staged parameter ends the block on, in plain units, for
    /// the listener notification after the render. Not cleared by the notify.
    ends: Vec<(u32, f32)>,
}

/// The stage both halves of a render share.
pub(crate) type SharedStage = Arc<AudioThreadCell<Stage>>;

impl Stage {
    /// A stage with its whole room reserved. Allocates: build it off the audio
    /// thread.
    pub(crate) fn shared() -> SharedStage {
        Arc::new(AudioThreadCell::new(Self {
            events: Vec::with_capacity(EVENT_ROOM),
            runs: Vec::with_capacity(RUN_ROOM),
            ends: Vec::with_capacity(RUN_ROOM),
        }))
    }

    /// Forgets the last block's staging.
    pub(crate) fn begin_block(&mut self) {
        self.events.clear();
        self.runs.clear();
        self.ends.clear();
    }

    /// Stages `queue` for parameter `id`, in plain units: a ramped event for
    /// each segment between two points at different offsets, and an immediate
    /// event for a lone point and for the value a step lands on. `false` when it does not fit, with nothing
    /// staged, for the caller to write the parameter at the head instead.
    pub(crate) fn stage(&mut self, queue: &ParameterQueue, bounds: ParamBounds) -> bool {
        let points = queue.points.len();
        if points == 0 {
            return true;
        }
        if self.runs.len() == RUN_ROOM || self.events.len() + points > EVENT_ROOM {
            return false;
        }
        let points = &queue.points[..];
        let offset = |i: usize| points[i].sample_offset.max(0);
        let plain = |i: usize| bounds.plain(points[i].value) as f32;
        for i in 0..points.len() {
            let next = i + 1;
            if next < points.len() && offset(next) > offset(i) {
                self.events.push(ParamEvent::Ramped {
                    start_buffer_offset: offset(i),
                    duration_frames: (offset(next) - offset(i)) as u32,
                    start_value: plain(i),
                    end_value: plain(next),
                });
            } else if next == points.len() && (i == 0 || offset(i - 1) == offset(i)) {
                self.events.push(ParamEvent::Immediate {
                    buffer_offset: offset(i) as u32,
                    value: plain(i),
                });
            }
        }
        let end = plain(points.len() - 1);
        let id = queue.param_id.get();
        self.runs.push((id, self.events.len()));
        self.ends.push((id, end));
        true
    }

    /// The parameters staged this block with the value each ends on.
    pub(crate) fn ends(&self) -> &[(u32, f32)] {
        &self.ends
    }

    /// Hands each staged parameter's events to `sink` and forgets them, so a
    /// render the processor did not stage for schedules nothing.
    fn drain(&mut self, mut sink: impl FnMut(u32, &[ParamEvent])) {
        let mut start = 0;
        for &(id, end) in &self.runs {
            sink(id, &self.events[start..end]);
            start = end;
        }
        self.events.clear();
        self.runs.clear();
    }

    /// Whether any event is staged and not yet handed over.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.events.len()
    }
}

/// The render notify's body: on the pre-render half, schedule what the
/// processor staged for this render.
pub(crate) fn schedule_pending(
    stage: &SharedStage,
    unit: &RenderUnit,
    notification: &RenderNotification,
) {
    if notification.phase != RenderPhase::Pre {
        return;
    }
    if let Some(mut stage) = stage.try_borrow_mut() {
        stage.drain(|id, events| {
            // The unit's refusal has nowhere to go: the audio thread cannot act
            // on it, and the parameter keeps the value it had.
            let _ = tutti_au_ffi::schedule(unit, id, ScheduleAddress::GLOBAL, events);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::ParamId;

    fn queue(id: u32, points: &[(i32, f64)]) -> ParameterQueue {
        let mut queue = ParameterQueue::with_capacity(ParamId::from(id), points.len());
        for &(offset, value) in points {
            assert!(queue.add_point(offset, value));
        }
        queue
    }

    fn drained(stage: &mut Stage) -> Vec<(u32, Vec<ParamEvent>)> {
        let mut runs = Vec::new();
        stage.drain(|id, events| runs.push((id, events.to_vec())));
        runs
    }

    /// A segment between two points at different offsets is one ramped event
    /// with its start and end values in the AU's plain units, its start offset
    /// and its length in frames; a lone point is an immediate event. Two
    /// parameters keep their own runs.
    #[test]
    fn a_segment_is_a_ramp_and_a_lone_point_is_immediate() {
        let stage = Stage::shared();
        let mut stage = stage.borrow_mut();
        let bounds = ParamBounds {
            min: 0.0,
            max: 100.0,
        };
        assert!(stage.stage(&queue(7, &[(3, 0.5), (25, 0.75), (30, 0.25)]), bounds));
        assert!(stage.stage(&queue(9, &[(4, 0.25)]), bounds));

        assert_eq!(
            drained(&mut stage),
            [
                (
                    7,
                    vec![
                        ParamEvent::Ramped {
                            start_buffer_offset: 3,
                            duration_frames: 22,
                            start_value: 50.0,
                            end_value: 75.0
                        },
                        ParamEvent::Ramped {
                            start_buffer_offset: 25,
                            duration_frames: 5,
                            start_value: 75.0,
                            end_value: 25.0
                        },
                    ]
                ),
                (
                    9,
                    vec![ParamEvent::Immediate {
                        buffer_offset: 4,
                        value: 25.0
                    }]
                ),
            ]
        );
        assert_eq!(stage.ends(), [(7, 25.0), (9, 25.0)]);
        assert!(drained(&mut stage).is_empty(), "a render schedules once");
    }

    /// Two points at one offset are a step: the later value lands there as an
    /// immediate event, and a ramp that follows starts from it.
    #[test]
    fn two_points_at_one_offset_are_a_step() {
        let stage = Stage::shared();
        let mut stage = stage.borrow_mut();
        let bounds = ParamBounds { min: 0.0, max: 1.0 };
        assert!(stage.stage(&queue(1, &[(0, 0.0), (10, 0.5), (10, 1.0)]), bounds));
        assert!(stage.stage(&queue(2, &[(10, 0.25), (10, 0.5), (30, 1.0)]), bounds));
        assert_eq!(
            drained(&mut stage),
            [
                (
                    1,
                    vec![
                        ParamEvent::Ramped {
                            start_buffer_offset: 0,
                            duration_frames: 10,
                            start_value: 0.0,
                            end_value: 0.5
                        },
                        ParamEvent::Immediate {
                            buffer_offset: 10,
                            value: 1.0
                        },
                    ]
                ),
                (
                    2,
                    vec![ParamEvent::Ramped {
                        start_buffer_offset: 10,
                        duration_frames: 20,
                        start_value: 0.5,
                        end_value: 1.0
                    }]
                ),
            ]
        );
    }

    /// A queue that does not fit stages nothing at all, so the caller writes
    /// its parameter at the head instead of half of its points landing.
    #[test]
    fn a_queue_that_does_not_fit_stages_nothing() {
        let stage = Stage::shared();
        let mut stage = stage.borrow_mut();
        let bounds = ParamBounds { min: 0.0, max: 1.0 };
        let mut too_many = ParameterQueue::with_capacity(ParamId::from(1), EVENT_ROOM + 1);
        for offset in 0..=EVENT_ROOM as i32 {
            assert!(too_many.add_point(offset, 0.5));
        }
        assert!(!stage.stage(&too_many, bounds));
        assert!(drained(&mut stage).is_empty());
        assert!(stage.ends().is_empty());
    }
}
