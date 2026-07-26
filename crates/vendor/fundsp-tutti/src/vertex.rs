//! Vertex structure for `Net`.

use super::audiounit::*;
use super::buffer::*;
use super::math::*;
use super::net::*;
use super::realnet::*;
use super::sequencer::Fade;
use super::*;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Clone)]
/// Individual AudioUnits are vertices in the graph.
pub(crate) struct Vertex {
    pub unit: Box<dyn AudioUnit>,
    pub source: Vec<NetEdge>,
    pub input: BufferVec,
    pub output: BufferVec,
    pub output_tmp: BufferVec,
    pub tick_input: Vec<f32>,
    pub tick_output: Vec<f32>,
    pub tick_output_tmp: Vec<f32>,
    pub id: NodeId,
    /// Current phase of fading into next node in 0...1.
    pub fade_phase: f32,
    /// Node we are fading into, if any. Not applicable to frontends.
    pub next: NodeEdit,
    /// The next node we will be fading into, if any. Not applicable to frontends.
    pub latest: NodeEdit,
    /// Units this vertex retired while the return queue was full.
    ///
    /// Retirement runs on the audio thread (`next_phase` from `tick`/`process`,
    /// `enqueue` from `NetBackend::handle_messages`), so a unit that cannot be
    /// handed back must be *kept* — dropping it would run
    /// `Box<dyn AudioUnit>::drop` in the callback and free an FDN reverb's delay
    /// lines under the deadline. Parked units are retried ahead of every later
    /// retirement, so the queue drains in retirement order once the frontend
    /// commits.
    ///
    /// Capacity is reserved in [`Self::allocate`]; pushing past it is the one
    /// case where retirement allocates, and it beats the alternative of freeing.
    pub retired: VecDeque<Box<dyn AudioUnit>>,
    /// If all vertex inputs are sourced from successive outputs of the indicated node,
    /// we can omit copying and use the source node outputs directly.
    pub source_vertex: Option<(NodeIndex, usize)>,
    /// Network revision in which this vertex was changed last.
    pub changed: u64,
    /// Used during order determination.
    pub unplugged: usize,
    /// Used during order determination.
    pub ordered: bool,
}

impl Vertex {
    pub fn new(id: NodeId, index: NodeIndex, unit: Box<dyn AudioUnit>) -> Self {
        let inputs = unit.inputs();
        let outputs = unit.outputs();
        let mut vertex = Self {
            unit,
            source: Vec::new(),
            input: BufferVec::new(inputs),
            output: BufferVec::new(outputs),
            output_tmp: BufferVec::new(outputs),
            tick_input: vec![0.0; inputs],
            tick_output: vec![0.0; outputs],
            tick_output_tmp: vec![0.0; outputs],
            id,
            fade_phase: 0.0,
            next: NodeEdit::default(),
            latest: NodeEdit::default(),
            retired: VecDeque::new(),
            source_vertex: None,
            changed: 0,
            unplugged: 0,
            ordered: false,
        };
        for i in 0..vertex.inputs() {
            vertex.source.push(edge(Port::Zero, Port::Local(index, i)));
        }
        vertex
    }

    #[inline]
    pub fn inputs(&self) -> usize {
        self.tick_input.len()
    }

    #[inline]
    pub fn outputs(&self) -> usize {
        self.tick_output.len()
    }

    pub fn allocate(&mut self) {
        self.unit.allocate();
        // A vertex holds at most `unit`, `next.unit` and `latest.unit`, so three
        // is the most that can retire before the frontend gets a chance to
        // commit and drain. Reserving here — on the control thread, from
        // `Net::allocate` — keeps the common overflow path push-only.
        self.retired.reserve(3);
    }

    /// Hand `unit` back to the frontend for deallocation, parking it in
    /// [`Self::retired`] if the return queue is full.
    ///
    /// Parked units go first: retirement order is preserved, and a queue that
    /// stays full cannot strand an early unit behind later ones.
    fn retire(&mut self, unit: Box<dyn AudioUnit>, sender: &Option<Arc<Queue<NetReturn, 256>>>) {
        let Some(sender) = sender else {
            // No frontend to return to. This is a standalone `Net`, where the
            // caller owns the units and no deadline applies.
            return;
        };
        // Flush the backlog oldest-first, so the frontend frees units in the
        // order they retired.
        while let Some(parked) = self.retired.pop_front() {
            if let Err(NetReturn::Unit(parked)) = sender.enqueue(NetReturn::Unit(parked)) {
                // Still full. Restore it and keep the rest parked behind it.
                self.retired.push_front(parked);
                break;
            }
        }
        // A non-empty backlog means the queue is still full, so this unit joins
        // the back of it rather than jumping the line.
        if !self.retired.is_empty() {
            self.retired.push_back(unit);
            return;
        }
        if let Err(NetReturn::Unit(unit)) = sender.enqueue(NetReturn::Unit(unit)) {
            self.retired.push_back(unit);
        }
    }

    pub fn update_source_vertex(&mut self) {
        self.source_vertex = None;
        if self.inputs() == 0 {
            return;
        }
        let mut source_node = 0;
        let mut source_port = 0;
        for i in 0..self.inputs() {
            let source = self.source[i].source;
            match source {
                Port::Local(node, port) => {
                    if i == 0 {
                        source_node = node;
                        source_port = port;
                    } else if source_node != node || source_port + i != port {
                        return;
                    }
                }
                _ => {
                    return;
                }
            }
        }
        self.source_vertex = Some((source_node, source_port));
    }

    /// We have faded to the next unit, now start fading to the latest unit, if any.
    fn next_phase(&mut self, sender: &Option<Arc<Queue<NetReturn, 256>>>) {
        let mut next = self.next.unit.take().unwrap();
        core::mem::swap(&mut self.unit, &mut next);
        self.retire(next, sender);
        self.next.fade = self.latest.fade.clone();
        self.fade_phase = 0.0;
        self.next.fade_time = self.latest.fade_time;
        core::mem::swap(&mut self.next.unit, &mut self.latest.unit);
    }

    #[inline]
    pub fn tick(&mut self, sample_rate: f32, sender: &Option<Arc<Queue<NetReturn, 256>>>) {
        self.unit.tick(&self.tick_input, &mut self.tick_output);

        if let Some(next) = self.next.unit.as_deref_mut() {
            let f = self.next.fade.at(1.0 - self.fade_phase);
            for x in self.tick_output.iter_mut() {
                *x *= f;
            }
            next.tick(&self.tick_input, &mut self.tick_output_tmp);
            let f = self.next.fade.at(self.fade_phase);
            for (x, y) in self.tick_output.iter_mut().zip(self.tick_output_tmp.iter()) {
                *x += *y * f;
            }
            self.fade_phase += 1.0 / (self.next.fade_time * sample_rate);
            if self.fade_phase >= 1.0 {
                self.next_phase(sender);
            }
        }
    }

    #[inline]
    pub fn process(
        &mut self,
        size: usize,
        input: &BufferRef,
        sample_rate: f32,
        sender: &Option<Arc<Queue<NetReturn, 256>>>,
    ) {
        self.unit
            .process(size, input, &mut self.output.buffer_mut());
        let outputs = self.outputs();
        if let Some(next) = self.next.unit.as_deref_mut() {
            let phase_left = ((1.0 - self.fade_phase) * self.next.fade_time * sample_rate) as usize;
            let n = min(size, phase_left);
            let fade_d = 1.0 / (self.next.fade_time * sample_rate);
            for channel in 0..outputs {
                let mut fade = self.fade_phase;
                match self.next.fade {
                    Fade::Power => {
                        for x in self.output.channel_f32_mut(channel)[..n].iter_mut() {
                            *x *= sine_ease(1.0 - fade);
                            fade += fade_d;
                        }
                    }
                    Fade::Smooth => {
                        for x in self.output.channel_f32_mut(channel)[..n].iter_mut() {
                            *x *= smooth5(1.0 - fade);
                            fade += fade_d;
                        }
                    }
                }
            }
            next.process(size, input, &mut self.output_tmp.buffer_mut());
            for channel in 0..self.outputs() {
                let mut fade = self.fade_phase;
                match self.next.fade {
                    Fade::Power => {
                        for (x, y) in self.output.channel_f32_mut(channel)[..n]
                            .iter_mut()
                            .zip(self.output_tmp.channel_f32(channel)[..n].iter())
                        {
                            *x += *y * sine_ease(fade);
                            fade += fade_d;
                        }
                    }
                    Fade::Smooth => {
                        for (x, y) in self.output.channel_f32_mut(channel)[..n]
                            .iter_mut()
                            .zip(self.output_tmp.channel_f32(channel)[..n].iter())
                        {
                            *x += *y * smooth5(fade);
                            fade += fade_d;
                        }
                    }
                }
                for (x, y) in self.output.channel_f32_mut(channel)[n..size]
                    .iter_mut()
                    .zip(self.output_tmp.channel_f32(channel)[n..size].iter())
                {
                    *x = *y;
                }
            }
            self.fade_phase += n as f32 / (self.next.fade_time * sample_rate);
            if phase_left <= size {
                // We don't start fading in the latest unit until the next block.
                self.next_phase(sender);
            }
        }
    }

    pub fn enqueue(&mut self, edit: &mut NodeEdit, sender: &Option<Arc<Queue<NetReturn, 256>>>) {
        if self.next.unit.is_some() {
            // Replace the latest unit.
            if let Some(latest) = self.latest.unit.take() {
                self.retire(latest, sender);
            }
            core::mem::swap(&mut self.latest, edit);
        } else {
            // Set the next unit.
            core::mem::swap(&mut self.next, edit);
            self.fade_phase = 0.0;
        }
    }
}
