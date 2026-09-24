//! The mono/stereo twins as they stood before design doc 013's rewrite-order
//! item 3, compiled **only for tests**, so the width-generic nodes that replace
//! them can be rendered side by side against them.
//!
//! Verbatim copies (test modules and module docs stripped). They exist for one
//! commit: the next converts the side-by-side tests into pinned golden values
//! and deletes this module.
#![allow(dead_code, clippy::all)]

pub(crate) mod chorus;
pub(crate) mod delay;
pub(crate) mod flanger;
pub(crate) mod ladder;
pub(crate) mod modulated_delay;
pub(crate) mod phaser;
pub(crate) mod shared;
pub(crate) mod svf;

mod equivalence;
