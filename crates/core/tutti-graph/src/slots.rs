//! The buffers an op touches, behind two small traits, so one body of op
//! code runs on both executors: the serial walk borrows straight out of the
//! [`Arena`] and the event slots (`split_at_mut`, as `arena.rs` explains),
//! and the parallel one out of a per-op view of them whose every slot is
//! **claimed** before use (`tutti_types::SplitRw`), so two ops the schedule
//! wrongly runs at once panic on the slot they share instead of aliasing
//! it. Neither needs `unsafe` here.
//!
//! The per-slot silence/constant flags are atomics in both modes (a load or
//! store of an `AtomicU8` with `Relaxed` is a plain byte move), so they need
//! no trait: the parallel executor shares one flag array between workers,
//! and a slot's claim orders its flag like its samples.

use std::sync::atomic::{AtomicU8, Ordering};

use tutti_types::RwView;

use crate::arena::{borrow_sorted, Arena, Line, Role};
use crate::event::Event;
use crate::plan::Direct;

/// A slot's flags.
#[inline]
pub(crate) fn flag_of(flags: &[AtomicU8], s: u32) -> u8 {
    flags[s as usize].load(Ordering::Relaxed)
}

/// Set a slot's flags.
#[inline]
pub(crate) fn set_flag(flags: &[AtomicU8], s: u32, v: u8) {
    flags[s as usize].store(v, Ordering::Relaxed);
}

/// The audio slots an op borrows: [`Arena`]'s own methods, by name.
pub(crate) trait AudioSlots {
    /// Slot `s`'s first `frames` samples.
    fn slot(&self, s: u32, frames: usize) -> &[f32];
    /// Slot `s`'s first `frames` samples, mutably.
    fn slot_mut(&mut self, s: u32, frames: usize) -> &mut [f32];
    /// `src` for reading and `dst` for writing, at once. They must differ.
    fn pair(&mut self, src: u32, dst: u32, frames: usize) -> (&[f32], &mut [f32]);
    /// A [`Direct`] node's buffers (see [`Arena::direct`]).
    fn direct<const I: usize, const O: usize>(
        &mut self,
        frames: usize,
        d: &Direct,
    ) -> ([&[f32]; I], [&mut [f32]; O]);
    /// A node's buffers by its sorted borrow requests (see
    /// [`Arena::borrow`]).
    fn borrow<'a>(
        &'a mut self,
        frames: usize,
        reqs: &[(u32, Role)],
        ins: &mut [&'a [f32]],
        outs: &mut [&'a mut [f32]],
    );
    /// Copy slot `src` over slot `dst`.
    fn copy_slot(&mut self, src: u32, dst: u32);
}

impl AudioSlots for Arena {
    #[inline(always)]
    fn slot(&self, s: u32, frames: usize) -> &[f32] {
        Arena::slot(self, s, frames)
    }
    #[inline(always)]
    fn slot_mut(&mut self, s: u32, frames: usize) -> &mut [f32] {
        Arena::slot_mut(self, s, frames)
    }
    #[inline(always)]
    fn pair(&mut self, src: u32, dst: u32, frames: usize) -> (&[f32], &mut [f32]) {
        Arena::pair(self, src, dst, frames)
    }
    #[inline(always)]
    fn direct<const I: usize, const O: usize>(
        &mut self,
        frames: usize,
        d: &Direct,
    ) -> ([&[f32]; I], [&mut [f32]; O]) {
        Arena::direct(self, frames, d)
    }
    #[inline(always)]
    fn borrow<'a>(
        &'a mut self,
        frames: usize,
        reqs: &[(u32, Role)],
        ins: &mut [&'a [f32]],
        outs: &mut [&'a mut [f32]],
    ) {
        Arena::borrow(self, frames, reqs, ins, outs)
    }
    #[inline(always)]
    fn copy_slot(&mut self, src: u32, dst: u32) {
        Arena::copy_slot(self, src, dst)
    }
}

/// The event slots an op borrows.
pub(crate) trait EventSlots {
    /// Slot `s`.
    fn ev(&self, s: u32) -> &Vec<Event>;
    /// Slot `s`, mutably.
    fn ev_mut(&mut self, s: u32) -> &mut Vec<Event>;
    /// Several slots at once, by requests sorted by slot (see
    /// `arena::borrow_sorted`).
    fn borrow_sorted<'a>(
        &'a mut self,
        reqs: &[(u32, Role)],
        read: impl FnMut(u8, &'a Vec<Event>),
        write: impl FnMut(u8, &'a mut Vec<Event>),
    );
}

impl EventSlots for [Vec<Event>] {
    #[inline(always)]
    fn ev(&self, s: u32) -> &Vec<Event> {
        &self[s as usize]
    }
    #[inline(always)]
    fn ev_mut(&mut self, s: u32) -> &mut Vec<Event> {
        &mut self[s as usize]
    }
    #[inline(always)]
    fn borrow_sorted<'a>(
        &'a mut self,
        reqs: &[(u32, Role)],
        mut read: impl FnMut(u8, &'a Vec<Event>),
        mut write: impl FnMut(u8, &'a mut Vec<Event>),
    ) {
        borrow_sorted(
            self,
            1,
            reqs,
            |port, v| read(port, &v[0]),
            |port, v| write(port, &mut v[0]),
        );
    }
}

/// [`EventSlots::borrow_sorted`] for requests built per call: sorted first.
#[inline]
pub(crate) fn borrow_events<'a, E: EventSlots + ?Sized>(
    events: &'a mut E,
    reqs: &mut [(u32, Role)],
    read: impl FnMut(u8, &'a Vec<Event>),
    write: impl FnMut(u8, &'a mut Vec<Event>),
) {
    reqs.sort_unstable();
    events.borrow_sorted(reqs, read, write);
}

/// Event slot `src` for reading and `dst` for writing, at once.
pub(crate) fn event_pair<E: EventSlots + ?Sized>(
    events: &mut E,
    src: u32,
    dst: u32,
) -> (&[Event], &mut Vec<Event>) {
    let mut reqs = [(src, Role::Read(0)), (dst, Role::Write(0))];
    let mut input: &[Event] = &[];
    let mut output: Option<&mut Vec<Event>> = None;
    borrow_events(events, &mut reqs, |_, v| input = v, |_, v| output = Some(v));
    (input, output.expect("dst borrowed"))
}

fn is_write(r: Role) -> bool {
    matches!(r, Role::Write(_))
}

/// One op's claimed view of the audio arena, for the parallel executor.
pub(crate) struct ArenaView<'v> {
    pub(crate) view: RwView<'v, Line>,
}

#[inline]
fn samples(lines: &[Line], frames: usize) -> &[f32] {
    &bytemuck::cast_slice::<Line, f32>(lines)[..frames]
}

#[inline]
fn samples_mut(lines: &mut [Line], frames: usize) -> &mut [f32] {
    &mut bytemuck::cast_slice_mut::<Line, f32>(lines)[..frames]
}

impl AudioSlots for ArenaView<'_> {
    #[inline]
    fn slot(&self, s: u32, frames: usize) -> &[f32] {
        samples(self.view.get(s as usize), frames)
    }
    #[inline]
    fn slot_mut(&mut self, s: u32, frames: usize) -> &mut [f32] {
        samples_mut(self.view.get_mut(s as usize), frames)
    }
    fn pair(&mut self, src: u32, dst: u32, frames: usize) -> (&[f32], &mut [f32]) {
        assert_ne!(src, dst, "pair() of one slot");
        let mut s: &[f32] = &[];
        let mut d: Option<&mut [f32]> = None;
        let reqs = if src < dst {
            [(src, Role::Read(0)), (dst, Role::Write(0))]
        } else {
            [(dst, Role::Write(0)), (src, Role::Read(0))]
        };
        self.view.split(
            &reqs,
            is_write,
            |_, l| s = samples(l, frames),
            |_, l| d = Some(samples_mut(l, frames)),
        );
        (s, d.expect("dst borrowed"))
    }
    fn direct<const I: usize, const O: usize>(
        &mut self,
        frames: usize,
        d: &Direct,
    ) -> ([&[f32]; I], [&mut [f32]; O]) {
        let n = usize::from(d.count);
        let mut reqs = [(0u32, 0u8); 4];
        for (k, r) in reqs.iter_mut().enumerate().take(n) {
            *r = (d.slots[k], d.role[k]);
        }
        let mut reads: [&[f32]; 4] = [&[]; 4];
        let mut outs: [&mut [f32]; O] = std::array::from_fn(|_| &mut [][..]);
        // Tag each request with its index into `slots`, so the reads land
        // where `d.input` looks for them.
        let mut tagged = [(0u32, (0u8, 0u8)); 4];
        for (k, t) in tagged.iter_mut().enumerate().take(n) {
            *t = (reqs[k].0, (k as u8, reqs[k].1));
        }
        self.view.split(
            &tagged[..n],
            |(_, role)| role != Direct::READ,
            |(k, _), l| reads[usize::from(k)] = samples(l, frames),
            |(_, role), l| outs[usize::from(role)] = samples_mut(l, frames),
        );
        let ins = std::array::from_fn(|c| match d.input[c] {
            Direct::IN_PLACE => &[][..],
            r => {
                assert_eq!(
                    d.role[usize::from(r)],
                    Direct::READ,
                    "a direct input reads a read slot"
                );
                reads[usize::from(r)]
            }
        });
        (ins, outs)
    }
    fn borrow<'a>(
        &'a mut self,
        frames: usize,
        reqs: &[(u32, Role)],
        ins: &mut [&'a [f32]],
        outs: &mut [&'a mut [f32]],
    ) {
        self.view.split(
            reqs,
            is_write,
            |r, l| {
                if let Role::Read(port) = r {
                    ins[port as usize] = samples(l, frames);
                }
            },
            |r, l| {
                if let Role::Write(port) = r {
                    outs[port as usize] = samples_mut(l, frames);
                }
            },
        );
    }
    fn copy_slot(&mut self, src: u32, dst: u32) {
        if src == dst {
            return;
        }
        let mut s: &[Line] = &[];
        let mut d: Option<&mut [Line]> = None;
        let reqs = if src < dst {
            [(src, Role::Read(0)), (dst, Role::Write(0))]
        } else {
            [(dst, Role::Write(0)), (src, Role::Read(0))]
        };
        self.view
            .split(&reqs, is_write, |_, l| s = l, |_, l| d = Some(l));
        d.expect("dst borrowed").copy_from_slice(s);
    }
}

/// One op's claimed view of the event slots, for the parallel executor.
pub(crate) struct EventsView<'v> {
    pub(crate) view: RwView<'v, Vec<Event>>,
}

impl EventSlots for EventsView<'_> {
    #[inline]
    fn ev(&self, s: u32) -> &Vec<Event> {
        &self.view.get(s as usize)[0]
    }
    #[inline]
    fn ev_mut(&mut self, s: u32) -> &mut Vec<Event> {
        &mut self.view.get_mut(s as usize)[0]
    }
    fn borrow_sorted<'a>(
        &'a mut self,
        reqs: &[(u32, Role)],
        mut read: impl FnMut(u8, &'a Vec<Event>),
        mut write: impl FnMut(u8, &'a mut Vec<Event>),
    ) {
        self.view.split(
            reqs,
            is_write,
            |r, v| {
                if let Role::Read(port) = r {
                    read(port, &v[0]);
                }
            },
            |r, v| {
                if let Role::Write(port) = r {
                    write(port, &mut v[0]);
                }
            },
        );
    }
}
