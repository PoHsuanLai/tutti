//! The data path of an Established Session: sequence numbers, forward error
//! correction, the retransmit buffer, reordering and the idle declaration.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tutti_ump::Ump;

use crate::{
    Command, RetransmitErrorReason, RetransmitRefusal, RetransmitRequest, SequenceNumber,
    SessionConfig, UmpData,
};

/// How far ahead of the expected number a command may be and still wait for
/// the commands between. Past it the gap is given up at once.
const REORDER_WINDOW: i16 = 256;

/// The interval at which Zero Length UMP Data Commands stop.
const IDLE_INTERVAL_STOP: Duration = Duration::from_secs(2);

/// A run of UMP Data Commands that never arrived and will not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Loss {
    pub(crate) first: SequenceNumber,
    pub(crate) count: u16,
}

#[derive(Clone, Copy, Debug)]
enum Idle {
    /// Data was not sent lately, or the idle period is over.
    Silent,
    /// Zero Length commands go out at `next`; `fec_left` more at the same
    /// interval cover the last data through forward error correction, then the
    /// interval doubles.
    Quiet {
        next: Instant,
        interval: Duration,
        fec_left: u8,
    },
}

#[derive(Clone, Copy, Debug)]
struct Gap {
    requests: u8,
    next_request: Instant,
}

/// What is known of the peer's retransmit support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerRetransmit {
    Presumed,
    Refused,
}

#[derive(Debug)]
pub(crate) struct Link {
    next_sequence: SequenceNumber,
    /// Sent commands, oldest first.
    history: VecDeque<UmpData>,
    idle: Idle,
    expected: SequenceNumber,
    /// Commands received ahead of `expected`, in no order.
    held: Vec<UmpData>,
    gap: Option<Gap>,
    peer_retransmit: PeerRetransmit,
}

impl Link {
    pub(crate) fn new() -> Self {
        Self {
            next_sequence: SequenceNumber::ZERO,
            history: VecDeque::new(),
            idle: Idle::Silent,
            expected: SequenceNumber::ZERO,
            held: Vec::new(),
            gap: None,
            peer_retransmit: PeerRetransmit::Presumed,
        }
    }

    /// The state of a Session just established: numbers at zero, buffers empty
    /// (M2-124 section 6.11).
    pub(crate) fn reset(&mut self) {
        let peer_retransmit = self.peer_retransmit;
        *self = Self::new();
        self.peer_retransmit = peer_retransmit;
    }

    fn remember(&mut self, cfg: &SessionConfig, command: UmpData) {
        self.history.push_back(command);
        let capacity = cfg
            .retransmit
            .capacity()
            .max(usize::from(cfg.fec.repeats()));
        while self.history.len() > capacity {
            self.history.pop_front();
        }
    }

    /// The commands a new one is sent behind: the last FEC repeat count sent, in
    /// the order sent.
    fn fec_prefix(&self, cfg: &SessionConfig, out: &mut Vec<Command>) {
        let skip = self
            .history
            .len()
            .saturating_sub(usize::from(cfg.fec.repeats()));
        out.extend(
            self.history
                .iter()
                .skip(skip)
                .cloned()
                .map(Command::UmpData),
        );
    }

    /// Queues `packets` as UMP Data Commands, behind the forward error
    /// correction repeats.
    pub(crate) fn send(
        &mut self,
        now: Instant,
        cfg: &SessionConfig,
        packets: &[Ump],
        out: &mut Vec<Command>,
    ) {
        let commands = UmpData::chunks(self.next_sequence, packets);
        if commands.is_empty() {
            return;
        }
        self.fec_prefix(cfg, out);
        for command in commands {
            self.next_sequence = command.sequence().next();
            out.push(Command::UmpData(command.clone()));
            self.remember(cfg, command);
        }
        self.idle = Idle::Quiet {
            next: now + cfg.idle_interval,
            interval: cfg.idle_interval,
            fec_left: cfg.fec.repeats(),
        };
    }

    pub(crate) fn idle_deadline(&self) -> Option<Instant> {
        match self.idle {
            Idle::Quiet { next, .. } => Some(next),
            Idle::Silent => None,
        }
    }

    /// Sends the Zero Length command that is due.
    pub(crate) fn on_idle(&mut self, now: Instant, cfg: &SessionConfig, out: &mut Vec<Command>) {
        let Idle::Quiet {
            next,
            interval,
            fec_left,
        } = self.idle
        else {
            return;
        };
        if now < next {
            return;
        }
        self.fec_prefix(cfg, out);
        let command = UmpData::idle(self.next_sequence);
        self.next_sequence = self.next_sequence.next();
        out.push(Command::UmpData(command.clone()));
        self.remember(cfg, command);
        self.idle = if fec_left > 0 {
            Idle::Quiet {
                next: now + interval,
                interval,
                fec_left: fec_left - 1,
            }
        } else if interval * 2 > IDLE_INTERVAL_STOP {
            Idle::Silent
        } else {
            Idle::Quiet {
                next: now + interval * 2,
                interval: interval * 2,
                fec_left: 0,
            }
        };
    }

    /// Answers a Retransmit Request from the commands kept: all of those asked
    /// for, or one Retransmit Error when the first is not kept.
    pub(crate) fn retransmit(&self, request: RetransmitRequest, out: &mut Vec<Command>) {
        let Some(oldest) = self.history.front().map(UmpData::sequence) else {
            out.push(Command::RetransmitError(RetransmitRefusal {
                reason: RetransmitErrorReason::BufferMiss,
                first: self.next_sequence,
            }));
            return;
        };
        let offset = request.first.distance_from(oldest);
        let Some(skip) = usize::try_from(offset)
            .ok()
            .filter(|&n| n < self.history.len())
        else {
            out.push(Command::RetransmitError(RetransmitRefusal {
                reason: RetransmitErrorReason::BufferMiss,
                first: oldest,
            }));
            return;
        };
        let take = if request.count == 0 {
            usize::MAX
        } else {
            usize::from(request.count)
        };
        out.extend(
            self.history
                .iter()
                .skip(skip)
                .take(take)
                .cloned()
                .map(Command::UmpData),
        );
    }

    /// Takes a received UMP Data Command: delivers it and any it unblocks in
    /// order, holds it when numbers are missing before it, drops it when it
    /// repeats one delivered.
    pub(crate) fn receive(
        &mut self,
        now: Instant,
        cfg: &SessionConfig,
        data: UmpData,
        delivered: &mut Vec<Ump>,
        lost: &mut Vec<Loss>,
    ) {
        let ahead = data.sequence().distance_from(self.expected);
        if ahead < 0 {
            return;
        }
        if ahead > 0 {
            if self.held.iter().any(|h| h.sequence() == data.sequence()) {
                return;
            }
            if ahead > REORDER_WINDOW {
                self.skip_to(data.sequence(), delivered, lost);
            } else {
                self.held.push(data);
                self.gap.get_or_insert(Gap {
                    requests: 0,
                    next_request: now + cfg.reorder_wait,
                });
                return;
            }
        }
        self.accept(&data, delivered);
        self.drain(delivered);
    }

    fn accept(&mut self, data: &UmpData, delivered: &mut Vec<Ump>) {
        self.expected = data.sequence().next();
        delivered.extend(data.packets());
    }

    /// Delivers the held commands that follow `expected` without a gap.
    fn drain(&mut self, delivered: &mut Vec<Ump>) {
        while let Some(at) = self.held.iter().position(|h| h.sequence() == self.expected) {
            let data = self.held.swap_remove(at);
            self.accept(&data, delivered);
        }
        if self.held.is_empty() {
            self.gap = None;
        }
    }

    /// The held command nearest ahead of `expected`.
    fn nearest_held(&self) -> Option<SequenceNumber> {
        self.held
            .iter()
            .map(UmpData::sequence)
            .min_by_key(|s| s.distance_from(self.expected))
    }

    /// Gives up the numbers from `expected` up to `target`.
    fn skip_to(&mut self, target: SequenceNumber, delivered: &mut Vec<Ump>, lost: &mut Vec<Loss>) {
        let count = target.distance_from(self.expected);
        if let Ok(count) = u16::try_from(count)
            && count > 0
        {
            lost.push(Loss {
                first: self.expected,
                count,
            });
            self.expected = target;
        }
        let expected = self.expected;
        self.held
            .retain(|h| h.sequence().distance_from(expected) >= 0);
        self.drain(delivered);
    }

    pub(crate) fn rx_deadline(&self) -> Option<Instant> {
        self.gap.map(|g| g.next_request)
    }

    /// Asks for the missing commands again, or gives them up when the peer
    /// cannot send them or the requests are spent.
    pub(crate) fn on_rx_timeout(
        &mut self,
        now: Instant,
        cfg: &SessionConfig,
        out: &mut Vec<Command>,
        delivered: &mut Vec<Ump>,
        lost: &mut Vec<Loss>,
    ) {
        let Some(gap) = self.gap else { return };
        if now < gap.next_request {
            return;
        }
        let Some(nearest) = self.nearest_held() else {
            self.gap = None;
            return;
        };
        if self.peer_retransmit == PeerRetransmit::Presumed && gap.requests < cfg.requests.limit() {
            let missing = u16::try_from(nearest.distance_from(self.expected)).unwrap_or(u16::MAX);
            out.push(Command::RetransmitRequest(RetransmitRequest {
                first: self.expected,
                count: missing,
            }));
            let requests = gap.requests + 1;
            let wait = cfg.reorder_wait * 2u32.saturating_pow(u32::from(requests));
            self.gap = Some(Gap {
                requests,
                next_request: now + wait,
            });
        } else {
            self.skip_to(nearest, delivered, lost);
        }
    }

    /// The peer cannot retransmit the numbers from `expected` to
    /// `error.first`: they are lost.
    pub(crate) fn on_retransmit_error(
        &mut self,
        error: RetransmitRefusal,
        delivered: &mut Vec<Ump>,
        lost: &mut Vec<Loss>,
    ) {
        let Some(nearest) = self.nearest_held() else {
            return;
        };
        let target = if error.first.distance_from(self.expected) > 0
            && error.first.distance_from(nearest) < 0
        {
            error.first
        } else {
            nearest
        };
        self.skip_to(target, delivered, lost);
    }

    /// The peer answered a Retransmit Request with a NAK for an unsupported
    /// command: no more are sent, and the gap in hand is given up at the next
    /// timeout.
    pub(crate) fn on_retransmit_refused(&mut self, now: Instant) {
        self.peer_retransmit = PeerRetransmit::Refused;
        if let Some(gap) = &mut self.gap {
            gap.next_request = now;
        }
    }
}
