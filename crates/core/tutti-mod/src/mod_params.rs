//! [`ModParams`] — a node/plugin declares its own **control-rate**-modulatable
//! params, returning a ready-made [`ModTarget`] a router accumulates into.
//!
//! The control-rate sibling of `tutti_units::ParamPorts` (which declares
//! audio-rate *ports*). A thing is control-rate-modulatable **iff** it implements
//! this trait — the enforced opt-in. It keys on [`ParamAddr`]
//! ([`UnitParam`](tutti_types::UnitParam) for native params, an opaque id for
//! foreign/plugin ones), so native nodes and plugins implement the *same* trait:
//! a native node answers on [`ParamAddr::Unit`] and returns `None` on
//! [`ParamAddr::Id`]; a plugin does the reverse.
//!
//! The trait lives here, in `tutti-mod`, because it is node-agnostic — it names
//! only [`ParamAddr`] (from `tutti-types`) and [`ModTarget`] (this crate). The
//! *impls* live in each node's own crate (`tutti-units`, `tutti-synth`,
//! `tutti-plugin`), which is why no single node crate owns the trait. `tutti-units`
//! re-exports it so existing `tutti_units::ModParams` users are unaffected.
//!
//! The node does **not** know its own `(base, min, max)` — that is DAW vocabulary
//! (a per-effect-kind table, app-side). The caller supplies it; the node/plugin
//! only knows *how to deliver* (a native node mirrors into its own `AtomicF32`;
//! a plugin accumulates locally and flushes over IPC).

use crate::target::ModTarget;
use std::sync::Arc;
use tutti_types::ParamAddr;

/// A node/plugin that exposes control-rate modulation for its scalar params.
///
/// Returns a [`ModTarget`] for `param` clamped to `[min, max]` around `base`, or
/// `None` if this node exposes no control-rate modulation for it. The target
/// writes its folded value into wherever the param actually lives (a native
/// node's `AtomicF32`, a plugin's IPC stream).
pub trait ModParams {
    fn mod_target(
        &self,
        param: ParamAddr,
        base: f32,
        min: f32,
        max: f32,
    ) -> Option<Arc<dyn ModTarget>>;
}
