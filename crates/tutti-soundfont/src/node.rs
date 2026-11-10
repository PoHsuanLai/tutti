//! The unit as a graph node: MIDI arrives on an
//! event input, on its frame (to the voices' 8-frame chunk, see
//! [`SoundFontUnit`]'s "How an event's offset is honoured").
//!
//! It follows its graph's rate: `prepare` rebuilds the synthesizer at the
//! prepared rate (control thread, where that may allocate), keeping the
//! preset.

use tutti_graph::{
    ConstantMask, Cx, IntoNode, Io, Node, NodeParts, Prepare, Resolution, Shape, SilenceMask,
    Status,
};
use tutti_types::ChannelLayout;

use crate::SoundFontUnit;
use crate::player::Plane;

impl Node for SoundFontUnit {
    /// No audio in, five out (dry left and right, chorus send left and right,
    /// reverb send), one MIDI event input, placed to the 8-frame chunk the
    /// voices render in, and the tail of the longest release.
    fn shape(&self) -> Shape {
        Shape::audio(
            ChannelLayout::EMPTY,
            ChannelLayout::from_count(Plane::COUNT as u16),
        )
        .with_events(1, 0)
        .with_event_resolution(Resolution::Frames(crate::SYNTH_BLOCK_FRAMES as u32))
        // Sample playback and filters per voice: measured 76 ns
        // per frame and channel with one note held, 140 with eight,
        // against `SvfFilterNode`'s 7.2 (doc 013, "The cost model and coarsening").
        // The default, stated.
        .with_cost(tutti_graph::Cost::Heavy)
        // The longest a voice rings after its key is up: an export that
        // ends at the last note-off extends by this, and keeps the
        // release. The effects after the unit declare their own.
        .with_tail(self.shape_tail())
    }

    /// Re-rate to the prepared rate (keeping the preset), and size the
    /// render scratch to the largest block. Control thread.
    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate();
        if rate.get().round() != self.sample_rate.get().round()
            && let Ok(unit) = self.with_sample_rate(rate)
        {
            *self = unit;
        }
        let frames = p.max_block().get();
        for plane in &mut self.planes {
            if plane.len() < frames {
                plane.resize(frames, 0.0);
            }
        }
    }

    /// What the block did: [`Status::Idle`] when no voice is left to sound and
    /// nothing was written; else [`Status::Masked`] naming the planes no voice
    /// sent anything to (exact zeros, written), and [`Status::Modified`] when
    /// every plane has something in it.
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        // No clamp: `prepare` sized the scratch to the prepared `MaxBlock`,
        // and the graph never hands a node a longer block.
        let size = io.frames();
        self.render_events(io.sub_blocks(0));
        let heard = self.take_heard();
        if self.is_idle() && !heard.has_any() {
            return Status::Idle;
        }
        let (_, mut outputs) = io.split();
        let mut silent = SilenceMask::NONE;
        for (c, (out, plane)) in outputs.iter_mut().zip(&self.planes).enumerate() {
            out[..size].copy_from_slice(&plane[..size]);
            if !heard.has(Plane::ALL[c]) {
                silent = silent.with(c);
            }
        }
        if silent == SilenceMask::NONE {
            Status::Modified
        } else {
            Status::Masked {
                silent,
                constant: ConstantMask::NONE,
            }
        }
    }

    fn reset(&mut self) {
        self.reset_player();
    }
}

/// The unit, inserted as a graph node: its fork is a graph node too, which
/// follows its render's rate (see the `fork` module docs). No controls: the
/// unit has no param a host sets while it plays (its preset is MIDI's).
impl IntoNode for SoundFontUnit {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        let fork = self.fork_template();
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(fork)),
        }
    }
}

/// A SoundFont unit is an instrument: MIDI on event input 0, its dry pair on
/// outputs 0 and 1. The other three outputs are the effect sends.
impl tutti_graph::Instrument for SoundFontUnit {
    fn main_outputs(&self) -> [u16; 2] {
        [Plane::DryLeft as u16, Plane::DryRight as u16]
    }
}
