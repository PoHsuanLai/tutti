//! Reusable plumbing with no plugin- or format-specific knowledge.
//!
//! - [`node`] — node primitives shared by every host path (in-process and
//!   out-of-process): the parameter-change sinks a plugin node reports
//!   through.
//! - [`window`] — platform window-handle plumbing for plugin editors.

pub mod node;
pub mod window;
