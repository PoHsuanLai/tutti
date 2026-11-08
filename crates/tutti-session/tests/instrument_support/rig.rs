//! A session with one instrument in it, played and rendered a block at a time.

use tutti_session::{InstrumentHandle, Session};
use tutti_types::{At, ChannelLayout, Frame, InterleavedMut, NodeKey, SampleRate, Samples};
use tutti_ump::Ump;

pub const RATE: SampleRate = SampleRate(48_000.0);
/// The frames a render call hands over; also the session's largest block.
pub const BLOCK: usize = 128;

pub struct Rig {
    pub session: Session,
    pub node: NodeKey,
    pub handle: InstrumentHandle,
    /// Left channel of everything rendered, from frame 0.
    pub left: Vec<f32>,
    /// Right channel.
    pub right: Vec<f32>,
    /// Real time to wait after each block: a hosted plugin in a live session
    /// answers a chunk in real time, and an unpaced render reads back silence.
    pub pace: std::time::Duration,
    /// The first frame a case looks at: what came before is warm-up.
    pub origin: usize,
}

pub fn offline_session() -> Session {
    Session::builder()
        .build_offline(RATE, ChannelLayout::STEREO, Samples(BLOCK))
        .expect("an offline session builds")
}

impl Rig {
    /// Makes `node` (already in `session`) an instrument and wires its strip
    /// to the outputs.
    pub fn new(mut session: Session, node: NodeKey) -> Result<Self, tutti_session::Error> {
        let handle = session.instrument(node)?;
        Ok(Self::wired(session, handle))
    }

    /// A rig over a handle the session already made.
    pub fn wired(mut session: Session, handle: InstrumentHandle) -> Self {
        session.graph().set_outputs_from(handle.out());
        session.update();
        Self {
            session,
            node: handle.node(),
            handle,
            left: Vec::new(),
            right: Vec::new(),
            pace: std::time::Duration::ZERO,
            origin: 0,
        }
    }

    /// Frames rendered so far: the position of the next block in this rig's
    /// own count. After a rate change it is not the engine's clock (see
    /// [`Rig::clock_of`]).
    pub fn frame(&self) -> usize {
        self.left.len()
    }

    /// Renders one block.
    pub fn block(&mut self) {
        let mut buf = vec![0.0f32; BLOCK * 2];
        self.session
            .render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        self.session.update();
        std::thread::sleep(self.pace);
        for f in buf.as_chunks::<2>().0 {
            self.left.push(f[0]);
            self.right.push(f[1]);
        }
    }

    /// Renders until `frame` frames exist.
    pub fn render_to(&mut self, frame: usize) {
        while self.frame() < frame {
            self.block();
        }
    }

    /// Renders until a re-prepare has landed, and one block after it so the
    /// instrument's queue has run on the new clock.
    pub fn settle(&mut self) {
        for _ in 0..64 {
            self.block();
            if !self.session.graph_ref().is_repreparing() {
                break;
            }
        }
        assert!(
            !self.session.graph_ref().is_repreparing(),
            "the re-prepare lands"
        );
        self.block();
    }

    /// `at` frames past the origin: where a case stamps.
    pub fn t(&self, at: usize) -> usize {
        self.origin + at
    }

    /// The engine's clock at this rig's frame `frame` (ahead of what is
    /// rendered). The clock counts frames at the current rate and a re-prepare
    /// at another rate moves it to the same wall-clock time, so it is not this
    /// rig's count: the two differ by what the rescale dropped. The queue's
    /// clock is the origin a frame is measured from.
    pub fn clock_of(&self, frame: usize) -> Frame {
        assert!(frame >= self.frame(), "frame {frame} is already rendered");
        Frame(self.handle.clock().get() + (frame - self.frame()) as u64)
    }

    /// Plays `ump` on this rig's frame `frame`, an absolute `At::Frame` on the
    /// engine's clock.
    pub fn at(&mut self, frame: usize, ump: Ump) {
        let at = At::Frame(self.clock_of(frame));
        self.handle.send(at, ump).expect("the queue has room");
    }

    /// First frame at or after `from` whose left or right sample is above
    /// `1e-6`.
    pub fn onset(&self, from: usize) -> Option<usize> {
        (from..self.left.len()).find(|&i| self.left[i].abs() > 1e-6 || self.right[i].abs() > 1e-6)
    }

    /// Largest absolute sample in `range` of either channel.
    pub fn peak(&self, range: std::ops::Range<usize>) -> f32 {
        range
            .map(|i| self.left[i].abs().max(self.right[i].abs()))
            .fold(0.0, f32::max)
    }

    /// Last frame whose sample is above `1e-4`, if any.
    pub fn last_audible(&self) -> Option<usize> {
        (0..self.left.len())
            .rev()
            .find(|&i| self.left[i].abs() > 1e-4 || self.right[i].abs() > 1e-4)
    }
}
