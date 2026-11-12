//! What a test of the session reads that no host should: the audio side, and
//! the plan the graph last sent. Behind the off-by-default `testing` feature,
//! which this crate's own tests turn on through a dev-dependency on itself.

use std::sync::Arc;

use tutti_graph::{Compensation, Editor, Executor, NodeKey, Prepare, Transport};

use super::{Graph, render};

impl Graph {
    /// What a test reads of the graph's plumbing: [`Testing`].
    pub fn testing(&self) -> Testing<'_> {
        Testing { graph: self }
    }

    /// The audio thread's half, for a test that renders what a device would
    /// hear: every commit lands on it, and ticking it plays the graph.
    ///
    /// # Panics
    ///
    /// If the audio side was already taken, by an earlier call or by the
    /// engine the session builder built over it.
    pub fn take_audio_side(&mut self) -> AudioSide {
        AudioSide::live(self.take_executor())
    }
}

/// The graph's plumbing as a test reads it: what the session drives and no
/// host calls. Made by [`Graph::testing`].
#[derive(Debug, Clone, Copy)]
pub struct Testing<'g> {
    graph: &'g Graph,
}

impl Testing<'_> {
    /// What every node is prepared for: the rate, the largest block, and the
    /// device's callback size when the session knows it.
    pub fn prepared(&self) -> Prepare {
        self.graph.prepared()
    }

    /// Whether a re-prepare is between its two commits.
    pub fn is_repreparing(&self) -> bool {
        self.graph.is_repreparing()
    }

    /// The plan last sent to the audio thread (`Editor::base`).
    pub fn sent_plan(&self) -> Option<Arc<tutti_graph::Plan>> {
        self.graph.editor.base().cloned()
    }

    /// How many commits have sent a plan.
    pub fn commits_sent(&self) -> u64 {
        self.graph.commits
    }

    /// The compensation of the plan sent last: what the session publishes,
    /// since it is what the executor is handed.
    pub fn sent_compensation(&self) -> Option<Compensation> {
        self.graph.sent_compensation()
    }

    /// Whether the graph keeps a reclaimer for `node`
    /// ([`SessionNode::reclaimer`](crate::SessionNode::reclaimer)): for a test that a path inserts a node
    /// through [`Graph::add`].
    pub fn keeps_reclaimer(&self, node: impl Into<NodeKey>) -> bool {
        let node = node.into();
        self.graph
            .nodes
            .get(&node)
            .is_some_and(|entry| entry.reclaimer.is_some())
    }

    /// How many crossfades wait for a re-prepare to resume.
    pub fn waiting_crossfades(&self) -> usize {
        self.graph.parked.len()
    }

    /// The compensation the next commit's plan carries: the spec compiled
    /// against the shapes, as the commit will compile it. Nothing is
    /// inserted — PDC is the compiler's. `None` when the spec does not
    /// compile (the commit will say why).
    pub fn planned_compensation(&self) -> Option<Compensation> {
        let valid = self.graph.editor.lowered().ok()?.spec.validate().ok()?;
        let (plan, _) = tutti_graph::compile(
            &valid,
            self.graph.editor.shapes(),
            self.graph.editor.prepare(),
            self.graph.editor.base().map(|p| &**p),
        )
        .ok()?;
        Some(super::compensation_of(&plan))
    }

    /// A live duplicate of the whole graph that shares no state with it
    /// (`Editor::fork`, `ForkMode::Live`), as an audio side to render. Each
    /// node is forked from its own fork source: a `ParamNode` from the
    /// values its set's cells hold. What an export renders from, so a test
    /// can check that a control write reaches it.
    ///
    /// # Errors
    ///
    /// When a node cannot be forked.
    pub fn fork_live(&self) -> Result<AudioSide, tutti_graph::ForkError> {
        let (editor, exec) = self.graph.editor.fork(
            tutti_graph::ForkTarget::Master,
            tutti_graph::ForkMode::Live,
            *self.graph.editor.prepare(),
        )?;
        Ok(AudioSide::forked(editor, exec))
    }
}

/// The audio thread's half of a graph, for a test that renders what a device
/// would hear: every commit lands on it, and rendering it plays the graph.
/// Taken with [`Graph::take_audio_side`] or [`Testing::fork_live`].
pub struct AudioSide {
    exec: Executor,
    transport: Transport,
    scratch: Vec<Vec<f32>>,
    /// A fork's own editor, kept for as long as its executor runs: the
    /// executor sends its boxes back to it. `None` for the live graph's
    /// audio side, whose editor stays in the graph.
    _editor: Option<Editor>,
}

impl std::fmt::Debug for AudioSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSide")
            .field("exec", &self.exec)
            .field("transport", &self.transport)
            .field("_editor", &self._editor)
            .finish_non_exhaustive()
    }
}

impl AudioSide {
    pub(super) fn live(exec: Executor) -> Self {
        Self {
            exec,
            transport: Transport::default(),
            scratch: Vec::new(),
            _editor: None,
        }
    }

    /// A fork's pair, rendered as an audio side.
    pub(super) fn forked(editor: Editor, exec: Executor) -> Self {
        Self {
            exec,
            transport: Transport::default(),
            scratch: Vec::new(),
            _editor: Some(editor),
        }
    }

    /// Renders one frame: `input` one sample per global input, `output` one
    /// per global output. Commits sent since the last call land first.
    pub fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        render(
            &mut self.exec,
            &mut self.scratch,
            &self.transport,
            input,
            output,
        );
    }

    /// The transport every block from here on renders under, as a device's
    /// engine hands its graph the playhead: a node that reads the transport
    /// from its `Env` (a placed sampler voice) plays on it. Stopped at beat
    /// zero until set.
    pub fn set_transport(&mut self, transport: Transport) {
        self.transport = transport;
    }

    /// Renders `frames` frames with no global input into `output`, planar,
    /// one slice per global output, in the blocks a device would hand over:
    /// executor blocks of up to `block` frames, each under the transport
    /// last [set](Self::set_transport) (stopped at beat zero until one is).
    ///
    /// # Panics
    ///
    /// If `block` is zero, or past the graph's prepared maximum.
    pub fn render(&mut self, frames: usize, block: usize, output: &mut [Vec<f32>]) {
        assert!(block > 0, "a zero-frame block");
        for o in output.iter_mut() {
            o.clear();
            o.resize(frames, 0.0);
        }
        let exec = &mut self.exec;
        let mut done = 0;
        while done < frames {
            let len = (frames - done).min(block);
            exec.apply_pending();
            let (ins, outs) = exec
                .plan()
                .map_or((0, 0), |p| (p.global_inputs() as usize, p.global_outputs()));
            let zeros = vec![0.0f32; len];
            let inputs: Vec<&[f32]> = (0..ins).map(|_| &zeros[..]).collect();
            let mut planes: Vec<Vec<f32>> = vec![vec![0.0; len]; outs];
            let mut slices: Vec<&mut [f32]> = planes.iter_mut().map(|p| &mut p[..]).collect();
            if exec.plan().is_some() {
                exec.process(len, &self.transport, &inputs, &mut slices);
            }
            for (o, p) in output.iter_mut().zip(&planes) {
                o[done..done + len].copy_from_slice(p);
            }
            done += len;
        }
    }
}
