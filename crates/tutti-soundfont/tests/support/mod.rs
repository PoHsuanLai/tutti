//! A SoundFont unit driven by hand the way a graph drives it: MIDI queued with
//! [`Hand::queue_midi`] is the next block's event input, each event on its
//! `frame_offset`, and a block is `Node::process` through
//! `tutti_graph::contract::Direct` (buffers built once, so the allocation
//! gates can run it).
//!
//! The node plays only its event input, so this is how a test feeds it MIDI.

#![allow(dead_code)] // each test binary uses its own subset

use std::ops::{Deref, DerefMut};

use tutti_core::{SampleRate, Samples};
use tutti_graph::contract::{Direct, HAND_EVENT_CAPACITY};
use tutti_graph::{Event, Offset};
use tutti_midi_types::ump::MidiEvent;
use tutti_soundfont::SoundFontUnit;

/// The block length a [`Hand`] renders: 64 frames.
pub const BLOCK: usize = 64;

/// A [`SoundFontUnit`] driven by hand. Derefs to the unit, so its own
/// methods (`note_on`, `program_change`, …) are reachable.
pub struct Hand {
    direct: Direct<SoundFontUnit>,
    queued: Vec<Event>,
}

impl Hand {
    /// `unit`, prepared at its own rate for [`BLOCK`]-frame blocks.
    pub fn new(unit: SoundFontUnit) -> Self {
        let rate = unit.sample_rate();
        Self::at(unit, rate)
    }

    /// `unit`, prepared at `rate` for [`BLOCK`]-frame blocks.
    pub fn at(unit: SoundFontUnit, rate: SampleRate) -> Self {
        Self {
            direct: Direct::new(unit, rate, BLOCK),
            queued: Vec::with_capacity(HAND_EVENT_CAPACITY),
        }
    }

    /// Play `events` in the next block, each on its `frame_offset` (clamped
    /// into the block). Allocation-free up to `HAND_EVENT_CAPACITY` a block.
    pub fn queue_midi(&mut self, events: &[MidiEvent]) {
        for e in events {
            let at = (e.frame_offset as usize).min(BLOCK - 1);
            let at = Offset::new(at, Samples(BLOCK)).expect("inside the block");
            self.queued.push(Event::midi(at, e.data));
        }
    }

    /// One block of `frames` (at most [`BLOCK`]): the queued events in it,
    /// stable-sorted by offset (an event past a shorter block lands on its
    /// last frame). `(left, right)`.
    pub fn block(&mut self, frames: usize) -> (&[f32], &[f32]) {
        self.direct.set_block_len(frames);
        let last = Offset::new(frames - 1, Samples(frames)).expect("inside");
        for e in &mut self.queued {
            if e.offset.index() >= frames {
                e.offset = last;
            }
        }
        // Stable (events on one frame keep their queued order) and
        // allocation-free: an insertion sort, not `sort_by_key`'s merge.
        for i in 1..self.queued.len() {
            let mut j = i;
            while j > 0 && self.queued[j - 1].offset > self.queued[j].offset {
                self.queued.swap(j - 1, j);
                j -= 1;
            }
        }
        self.direct.events(0, &self.queued);
        self.queued.clear();
        self.direct.block();
        (self.direct.output(0), self.direct.output(1))
    }

    /// One frame (a one-frame block): `[left, right]`.
    pub fn tick(&mut self) -> [f32; 2] {
        let (l, r) = self.block(1);
        [l[0], r[0]]
    }
}

impl Deref for Hand {
    type Target = SoundFontUnit;
    fn deref(&self) -> &SoundFontUnit {
        &self.direct.node
    }
}

impl DerefMut for Hand {
    fn deref_mut(&mut self) -> &mut SoundFontUnit {
        &mut self.direct.node
    }
}
