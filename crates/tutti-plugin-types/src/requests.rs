//! How a plugin's calls into the host, and the host's edits to a running
//! plugin, cross threads.
//!
//! A plugin calls its host from any thread. Each such call is one bit of a
//! [`RequestWord`]: [`Request::bit`] names it, the
//! callback raises it, and the main thread's idle pass turns the taken bits
//! back into [`Request`]s ([`taken`]) and feeds them to the core. Timers and
//! file descriptors carry an id, so they are main-thread registrations and
//! have no bit.
//!
//! An [`Edit`] goes the other way: a parameter write on an active plugin,
//! queued to the Processor and applied at the head of its next block.

use tutti_rt::{EditReader, EditWriter, RequestBit, RequestWord, RingFull, edit_ring};

use crate::{Normalized, ParamEdit, ParamId, Request, RescanScope};

const RESTART: RequestBit = RequestBit::of(0);
const IO_CHANGED: RequestBit = RequestBit::of(1);
const LATENCY_CHANGED: RequestBit = RequestBit::of(2);
const TAIL_CHANGED: RequestBit = RequestBit::of(3);
const RELOAD: RequestBit = RequestBit::of(4);
const CALLBACK: RequestBit = RequestBit::of(5);
const RESCAN_VALUES: RequestBit = RequestBit::of(6);
const RESCAN_TEXT: RequestBit = RequestBit::of(7);
const RESCAN_INFO: RequestBit = RequestBit::of(8);
const RESCAN_ALL: RequestBit = RequestBit::of(9);
const MIDI_CC: RequestBit = RequestBit::of(10);
const NOTE_EXPRESSION: RequestBit = RequestBit::of(11);

/// Every request that has a bit, in the order [`taken`] yields them.
const WORD_REQUESTS: [(RequestBit, Request); 12] = [
    (RESTART, Request::Restart),
    (IO_CHANGED, Request::IoChanged),
    (LATENCY_CHANGED, Request::LatencyChanged),
    (TAIL_CHANGED, Request::TailChanged),
    (RELOAD, Request::Reload),
    (CALLBACK, Request::Callback),
    (RESCAN_VALUES, Request::RescanParams(RescanScope::Values)),
    (RESCAN_TEXT, Request::RescanParams(RescanScope::Text)),
    (RESCAN_INFO, Request::RescanParams(RescanScope::Info)),
    (RESCAN_ALL, Request::RescanParams(RescanScope::All)),
    (MIDI_CC, Request::MidiCcAssignmentChanged),
    (NOTE_EXPRESSION, Request::NoteExpressionChanged),
];

impl Request {
    /// The bit a callback raises for this request, or `None` for a timer or a
    /// file descriptor, which carry an id and are registered on the main
    /// thread.
    #[must_use]
    pub fn bit(self) -> Option<RequestBit> {
        WORD_REQUESTS
            .iter()
            .find_map(|&(bit, request)| (request == self).then_some(bit))
    }

    /// Raises this request on `word`. Wait-free; any thread. `false` when the
    /// request has no bit and nothing was raised.
    pub fn raise(self, word: &RequestWord) -> bool {
        match self.bit() {
            Some(bit) => {
                word.raise(bit);
                true
            }
            None => false,
        }
    }
}

/// The requests raised on `word` since the last call, each once. Main thread
/// only: it takes the word.
pub fn taken(word: &RequestWord) -> impl Iterator<Item = Request> + use<> {
    let bits = word.take();
    WORD_REQUESTS
        .into_iter()
        .filter_map(move |(bit, request)| bits.contains(bit).then_some(request))
}

/// A change the main thread queues to a running plugin.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Edit {
    /// Set a parameter.
    Param {
        /// Which.
        id: ParamId,
        /// To what.
        value: Normalized,
    },
    /// Every write queued before this one is stale: a state or a preset load
    /// replaced the values they were written against. A drain applies none of
    /// the writes that precede it, which the ring's order makes exact.
    Reset,
    /// A parameter edit the plugin reported, on its way from the audio thread
    /// to the control side: a ring the other way round carries it (a format
    /// whose plugin reports from `process` and `params.flush` ends it here, and
    /// the Controller's `take_reported_edits` reads it).
    Reported(ParamEdit),
}

const PARAM_TAG: u64 = 1;
const RESET_TAG: u64 = 2;
const BEGIN_TAG: u64 = 3;
const CHANGE_TAG: u64 = 4;
const END_TAG: u64 = 5;
/// Set in a reported edit's tag when it is not to be recorded.
const UNRECORDED: u64 = 0x100;

impl Edit {
    pub(crate) fn encode(self) -> [u64; 2] {
        match self {
            Self::Param { id, value } => {
                [PARAM_TAG << 32 | u64::from(id.get()), value.get().to_bits()]
            }
            Self::Reset => [RESET_TAG << 32, 0],
            Self::Reported(edit) => {
                let (tag, id, value) = match edit {
                    ParamEdit::Begin(id) => (BEGIN_TAG, id, 0),
                    ParamEdit::Change { id, value, record } => (
                        if record {
                            CHANGE_TAG
                        } else {
                            CHANGE_TAG | UNRECORDED
                        },
                        id,
                        value.get().to_bits(),
                    ),
                    ParamEdit::End(id) => (END_TAG, id, 0),
                };
                [tag << 32 | u64::from(id.get()), value]
            }
        }
    }

    pub(crate) fn decode(raw: [u64; 2]) -> Option<Self> {
        match raw[0] >> 32 {
            PARAM_TAG => Some(Self::Param {
                id: ParamId::new(raw[0] as u32),
                value: Normalized::clamped(f64::from_bits(raw[1])),
            }),
            RESET_TAG => Some(Self::Reset),
            tag => {
                let id = ParamId::new(raw[0] as u32);
                Some(Self::Reported(match tag {
                    BEGIN_TAG => ParamEdit::Begin(id),
                    END_TAG => ParamEdit::End(id),
                    CHANGE_TAG => ParamEdit::Change {
                        id,
                        value: Normalized::clamped(f64::from_bits(raw[1])),
                        record: true,
                    },
                    t if t == CHANGE_TAG | UNRECORDED => ParamEdit::Change {
                        id,
                        value: Normalized::clamped(f64::from_bits(raw[1])),
                        record: false,
                    },
                    _ => return None,
                }))
            }
        }
    }
}

/// A ring of [`Edit`]s holding up to `capacity`, as its sending and draining
/// ends.
///
/// # Panics
///
/// If `capacity` is zero.
#[must_use]
pub fn edits(capacity: usize) -> (EditSink, EditDrain) {
    let (writer, reader) = edit_ring(capacity);
    (
        EditSink(writer),
        EditDrain {
            reader,
            undecodable: 0,
        },
    )
}

/// The main thread's end of an edit ring.
#[derive(Debug)]
pub struct EditSink(EditWriter<[u64; 2]>);

impl EditSink {
    /// Queues `edit`.
    ///
    /// # Errors
    ///
    /// [`RingFull`], counted in [`refused`](Self::refused). The caller keeps
    /// the latest value per parameter and retries on its next tick.
    pub fn send(&mut self, edit: Edit) -> Result<(), RingFull> {
        self.0.push(edit.encode())
    }

    /// Edits refused for want of room.
    #[must_use]
    pub fn refused(&self) -> u32 {
        self.0.refused()
    }
}

/// The main thread's side of the edits a Controller hands its Processor: an
/// [`EditSink`], and the writes the ring refused, kept until a tick retries them.
///
/// A write to a parameter already waiting replaces it (latest value per
/// parameter), and nothing is sent ahead of a [`Edit::Reset`] the full ring
/// refused. One idea for every format whose Controller queues writes.
#[derive(Debug)]
pub struct EditQueue {
    sink: EditSink,
    /// Writes a full ring refused, latest per parameter, oldest first.
    unsent: Vec<(ParamId, Normalized)>,
    /// A [`Edit::Reset`] the full ring refused: it goes before anything in
    /// `unsent`, which is everything written since.
    reset_pending: bool,
}

impl EditQueue {
    /// A queue over `sink`, with nothing waiting.
    #[must_use]
    pub fn new(sink: EditSink) -> Self {
        Self {
            sink,
            unsent: Vec::new(),
            reset_pending: false,
        }
    }

    /// Queues a write. `true` when it entered the ring now; `false` when it is
    /// kept for the next [`retry`](Self::retry) (the ring is full, a reset or an
    /// earlier write to the parameter is waiting).
    pub fn queue(&mut self, id: ParamId, value: Normalized) -> bool {
        let held_back = self.reset_pending || self.unsent.iter().any(|(waiting, _)| *waiting == id);
        let sent = !held_back && self.sink.send(Edit::Param { id, value }).is_ok();
        if !sent {
            match self.unsent.iter_mut().find(|(waiting, _)| *waiting == id) {
                Some(slot) => slot.1 = value,
                None => self.unsent.push((id, value)),
            }
        }
        sent
    }

    /// Makes every write made so far stale: a state or a preset load replaced
    /// the values they were written against, and the next block must not put
    /// them back. The ring's order makes a [`Edit::Reset`] exact; a full ring
    /// holds it for the next tick, and every write after it waits behind it.
    pub fn reset(&mut self) {
        self.unsent.clear();
        self.reset_pending = self.sink.send(Edit::Reset).is_err();
    }

    /// Retries what the ring refused, oldest first, and calls `sent` for each
    /// write that entered it.
    pub fn retry(&mut self, mut sent: impl FnMut(ParamId, Normalized)) {
        if self.reset_pending {
            if self.sink.send(Edit::Reset).is_err() {
                return;
            }
            self.reset_pending = false;
        }
        for (id, value) in std::mem::take(&mut self.unsent) {
            if self.sink.send(Edit::Param { id, value }).is_err() {
                self.unsent.push((id, value));
            } else {
                sent(id, value);
            }
        }
    }

    /// The writes waiting for a retry, taken: a rejoin carries them on.
    pub fn take_unsent(&mut self) -> Vec<(ParamId, Normalized)> {
        std::mem::take(&mut self.unsent)
    }

    /// Edits the ring refused for want of room, cumulatively.
    #[must_use]
    pub fn refused(&self) -> u32 {
        self.sink.refused()
    }
}

/// The audio thread's end of an edit ring.
#[derive(Debug)]
pub struct EditDrain {
    reader: EditReader<[u64; 2]>,
    undecodable: u32,
}

impl EditDrain {
    /// Hands each edit queued when the call began to `apply`, oldest first;
    /// what the producer pushes meanwhile waits for the next block, so a hot
    /// producer cannot hold the audio thread. Wait-free; never allocates.
    ///
    /// An entry that decodes to no known [`Edit`] is dropped and counted in
    /// [`undecodable`](Self::undecodable): only a producer of another version
    /// could write one.
    pub fn drain(&mut self, mut apply: impl FnMut(Edit)) {
        for _ in 0..self.reader.pending() {
            let Some(raw) = self.reader.pop() else {
                break;
            };
            match Edit::decode(raw) {
                Some(edit) => apply(edit),
                None => self.undecodable = self.undecodable.saturating_add(1),
            }
        }
    }

    /// Entries dropped because they decoded to no known edit.
    #[must_use]
    pub fn undecodable(&self) -> u32 {
        self.undecodable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request with a bit round-trips through the word, once each, and a
    /// timer or descriptor has none.
    #[test]
    fn every_request_with_a_bit_is_taken_once() {
        let word = RequestWord::new();
        for &(_, request) in &WORD_REQUESTS {
            assert!(request.raise(&word));
            assert!(request.raise(&word), "raising twice is one request");
        }
        assert!(!Request::Timer(crate::TimerId(1)).raise(&word));
        assert!(!Request::Fd(crate::FdId(3)).raise(&word));
        let got: Vec<_> = taken(&word).collect();
        let want: Vec<_> = WORD_REQUESTS.iter().map(|&(_, r)| r).collect();
        assert_eq!(got, want);
        assert_eq!(taken(&word).count(), 0);
    }

    /// An edit survives the ring's encoding, in order; a full ring refuses and
    /// counts.
    #[test]
    fn edits_round_trip_in_order() {
        let (mut sink, mut drain) = edits(2);
        let a = Edit::Param {
            id: ParamId::new(7),
            value: Normalized::of(0.25),
        };
        let b = Edit::Param {
            id: ParamId::new(u32::MAX),
            value: Normalized::of(1.0),
        };
        sink.send(a).unwrap();
        sink.send(b).unwrap();
        assert!(sink.send(a).is_err());
        assert_eq!(sink.refused(), 1);
        let mut got = Vec::new();
        drain.drain(|e| got.push(e));
        assert_eq!(got, [a, b]);
    }

    /// An entry with an unknown tag is dropped and counted, and the edits
    /// around it still arrive.
    ///
    /// A `Reset` survives the ring in order, between the writes it separates.
    #[test]
    fn a_reset_round_trips_between_writes() {
        let (mut sink, mut drain) = edits(4);
        let a = Edit::Param {
            id: ParamId::new(1),
            value: Normalized::of(0.5),
        };
        sink.send(a).unwrap();
        sink.send(Edit::Reset).unwrap();
        sink.send(a).unwrap();
        let mut got = Vec::new();
        drain.drain(|e| got.push(e));
        assert_eq!(got, [a, Edit::Reset, a]);
        assert_eq!(drain.undecodable(), 0);
    }

    /// A reported edit survives the ring in order: each gesture end, a change
    /// with its value to the bit, and the flag of one not to be recorded.
    #[test]
    fn reported_edits_round_trip_in_order() {
        let (mut sink, mut drain) = edits(8);
        let id = ParamId::new(u32::MAX);
        let sent = [
            Edit::Reported(ParamEdit::Begin(id)),
            Edit::Reported(ParamEdit::change(id, Normalized::of(0.3))),
            Edit::Reported(ParamEdit::change(ParamId::new(2), Normalized::of(0.7)).unrecorded()),
            Edit::Reported(ParamEdit::End(id)),
        ];
        for edit in sent {
            sink.send(edit).unwrap();
        }
        let mut got = Vec::new();
        drain.drain(|e| got.push(e));
        assert_eq!(got, sent);
        assert_eq!(drain.undecodable(), 0);
    }

    #[test]
    fn an_unknown_entry_is_dropped_and_counted() {
        assert_eq!(Edit::decode([99 << 32, 0]), None);
        let (mut writer, reader) = edit_ring::<[u64; 2]>(4);
        let good = Edit::Param {
            id: ParamId::new(1),
            value: Normalized::of(0.5),
        };
        writer.push(good.encode()).unwrap();
        writer.push([99 << 32, 0]).unwrap();
        writer.push(good.encode()).unwrap();
        let mut drain = EditDrain {
            reader,
            undecodable: 0,
        };
        let mut got = 0;
        drain.drain(|_| got += 1);
        assert_eq!(got, 2);
        assert_eq!(drain.undecodable(), 1);
    }

    /// A drain applies what was queued when it began, not what arrives during
    /// it: a producer that pushes from inside `apply` cannot extend the call.
    #[test]
    fn a_drain_is_bounded_by_what_was_queued() {
        let (mut sink, mut drain) = edits(8);
        let edit = Edit::Param {
            id: ParamId::new(1),
            value: Normalized::of(0.5),
        };
        sink.send(edit).unwrap();
        sink.send(edit).unwrap();
        let mut applied = 0;
        drain.drain(|_| {
            applied += 1;
            let _ = sink.send(edit);
        });
        assert_eq!(applied, 2);
        let mut rest = 0;
        drain.drain(|_| rest += 1);
        assert_eq!(
            rest, 2,
            "the pushes made during the drain wait for the next"
        );
    }
}
