//! `Editor::set_limits`: a host's bounds, enforced where commits are made.
//!
//! The engine sets these to its fold scratch (`MAX_ROOT_CHANNELS` global
//! outputs, its block capacity); a graph past them must be refused on the
//! control thread rather than reach an audio callback that cannot run it.

use tutti_graph::{CommitError, Cx, Editor, Io, Limits, Node, Prepare, Shape, Status};
use tutti_types::graph::{OutPort, Source};
use tutti_types::{ChannelLayout, NodeKey, SampleRate, Samples};

struct Wide(u16);

impl Node for Wide {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::from_count(self.0))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn prep(max: usize) -> Prepare {
    Prepare::new(SampleRate(48_000.0), Samples(max))
}

fn wire(ed: &mut Editor, width: u16) {
    ed.spec_mut().topology.outputs = (0..width)
        .map(|port| {
            Source::Node(OutPort {
                node: NodeKey(1),
                port,
            })
        })
        .collect();
}

const LIMITS: Limits = Limits {
    max_global_outputs: 8,
    max_block: 1024,
};

/// Past the limits, a commit and a re-prepare are refused and nothing is
/// sent; within them, both go through.
///
/// Mutation: drop the output check from `commit` → the 10-output commit is
/// sent → fails. Drop the block check from `reprepare` → the 2048-frame
/// re-prepare starts → fails.
#[test]
fn commits_and_re_prepares_past_the_limits_are_refused() {
    let (mut ed, mut exec) = Editor::new(prep(512));
    ed.set_limits(LIMITS).expect("nothing sent yet");
    ed.insert(NodeKey(1), "wide", Wide(10));
    wire(&mut ed, 10);
    assert_eq!(
        ed.commit(),
        Err(CommitError::TooManyOutputs {
            outputs: 10,
            limit: 8
        })
    );
    assert_eq!(ed.in_flight(), 0, "nothing was sent");

    wire(&mut ed, 8);
    ed.commit().expect("eight fit");
    exec.apply_pending();
    ed.collect();

    assert_eq!(
        ed.reprepare(prep(2048)),
        Err(CommitError::BlockTooLong {
            max_block: 2048,
            limit: 1024
        })
    );
    assert_eq!(ed.in_flight(), 0);
    // A re-prepare commits the spec as it stands, so it is checked too.
    wire(&mut ed, 9);
    assert!(matches!(
        ed.reprepare(prep(1024)),
        Err(CommitError::TooManyOutputs { .. })
    ));
    wire(&mut ed, 8);
    ed.reprepare(prep(1024)).expect("within the limits");
}

/// Limits that what was already sent breaks are refused, and change
/// nothing.
///
/// Mutation: skip the sent-plan check in `set_limits` → accepted → fails.
/// Skip the `Prepare` check → the 512 limit is accepted over a 1024
/// `MaxBlock` → fails.
#[test]
fn limits_below_what_was_sent_are_refused() {
    let (mut ed, _exec) = Editor::new(prep(1024));
    ed.insert(NodeKey(1), "wide", Wide(10));
    wire(&mut ed, 10);
    ed.commit().expect("no limits yet");
    assert!(matches!(
        ed.set_limits(LIMITS),
        Err(CommitError::TooManyOutputs { outputs: 10, .. })
    ));
    assert_eq!(ed.limits(), Limits::NONE);

    let (mut ed, _exec) = Editor::new(prep(1024));
    assert!(matches!(
        ed.set_limits(Limits {
            max_global_outputs: 8,
            max_block: 512
        }),
        Err(CommitError::BlockTooLong { .. })
    ));
}

/// An editor knows its own executor.
///
/// Mutation: make `same_pair` return `true` → fails.
#[test]
fn an_editor_knows_its_executor() {
    let (ed, exec) = Editor::new(prep(64));
    let (_other, stranger) = Editor::new(prep(64));
    assert!(ed.is_paired_with(&exec));
    assert!(!ed.is_paired_with(&stranger));
}

/// Limits only tighten: a looser (or no) limit after a host set one changes
/// nothing, so no caller can let through a graph the host cannot run.
///
/// Mutation (run): take `limits` as given in `set_limits` (no per-field min)
/// → `Limits::NONE` lifts the bound and the 10-output commit is sent →
/// fails.
#[test]
fn limits_only_tighten() {
    let (mut ed, _exec) = Editor::new(prep(512));
    ed.set_limits(LIMITS).expect("nothing sent yet");
    ed.set_limits(Limits::NONE).expect("a no-op");
    assert_eq!(ed.limits(), LIMITS);
    ed.set_limits(Limits {
        max_global_outputs: 4,
        max_block: 4096,
    })
    .expect("tightens one field");
    assert_eq!(
        ed.limits(),
        Limits {
            max_global_outputs: 4,
            max_block: 1024
        }
    );
    ed.insert(NodeKey(1), "wide", Wide(10));
    wire(&mut ed, 10);
    assert!(matches!(
        ed.commit(),
        Err(CommitError::TooManyOutputs { limit: 4, .. })
    ));
}
