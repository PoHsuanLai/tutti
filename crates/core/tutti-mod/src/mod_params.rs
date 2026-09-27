//! [`ModParams`] — a node/plugin declares its own **control-rate**-modulatable
//! params, returning a ready-made [`ModTarget`] a router accumulates into.
//!
//! The control-rate sibling of a node's declared modulatable params (which
//! the graph modulates per frame; `tutti_graph::Shape::params`). A
//! thing is control-rate-modulatable **iff** it implements
//! this trait — the enforced opt-in. It keys on [`ParamAddr`]
//! ([`UnitParam`](tutti_types::UnitParam) for native params, an opaque id for
//! foreign/plugin ones), so native nodes and plugins implement the *same* trait:
//! a native node answers on [`ParamAddr::Unit`] and returns `None` on
//! [`ParamAddr::Id`]; a plugin does the reverse.
//!
//! The trait lives here, in `tutti-mod`, because it is node-agnostic — it names
//! only [`ParamAddr`] (from `tutti-types`) and [`ModTarget`] (this crate). The
//! *impls* live in each node's own crate (`tutti-nodes`, `tutti-polysynth`,
//! `tutti-plugin`), which is why no single node crate owns the trait.
//! `tutti-nodes` re-exports it.
//!
//! The node does **not** know its own `(base, min, max)` — that is DAW vocabulary
//! (a per-effect-kind table, app-side). The caller supplies it; the node/plugin
//! only knows *how to deliver* (a native node mirrors into its own `AtomicF32`;
//! a plugin accumulates locally and flushes over IPC).

use crate::target::ModTarget;
use std::sync::Arc;
use tutti_types::ParamAddr;

/// A node or plugin that exposes control-rate (per-frame) modulation for its
/// scalar params.
///
/// Implementing it is the opt-in: a thing is control-rate-modulatable exactly
/// when it implements this trait. Native nodes and plugins implement the same
/// trait, keyed on [`ParamAddr`]. [`mod_target`](Self::mod_target) returns a
/// [`ModTarget`] that writes its folded value to wherever the param actually
/// lives (a native node's `AtomicF32`, a plugin's IPC stream).
///
/// The node does not know its own `(base, min, max)`; the caller supplies
/// them. Implementations live in the node crates (`tutti-nodes`,
/// `tutti-polysynth`, `tutti-plugin`); `tutti-nodes` re-exports the trait.
/// Requires the `routing` feature.
pub trait ModParams {
    /// Returns the [`ModTarget`] for `param`, clamped to `[min, max]` around
    /// `base`, or `None` if this node exposes no control-rate modulation for
    /// that address.
    ///
    /// `base`, `min` and `max` are the caller's. They are bare `f32` because they are in
    /// the *param's* units, which differ per address: `Hz` for a cutoff, linear
    /// gain for a fader, `Semitones` for a pitch. No one newtype is right for
    /// the triple.
    ///
    /// Which [`ParamAddr`] arm to answer on is the whole addressing contract:
    /// a native node answers on [`ParamAddr::Unit`] (an engine-known
    /// [`UnitParam`](tutti_types::UnitParam)) and returns `None` on
    /// [`ParamAddr::Id`]; a plugin or WASM node does the reverse, because its
    /// param names are exactly what the app's name table cannot know.
    /// Answering on the wrong arm binds no route and reports no error.
    fn mod_target(
        &self,
        param: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>>;
}
