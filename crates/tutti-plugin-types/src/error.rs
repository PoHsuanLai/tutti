//! Format-agnostic plugin error, returned by the hosting traits
//! ([`Control`](crate::Control), [`Processor`](crate::Processor)).
//!
//! This is the *lean* error the shared traits speak — it carries only the
//! failure modes a format loader can produce (load, state, process, editor),
//! never the IPC/host-coupled variants (`ServerNotFound`, `ProtocolMismatch`,
//! `IpcError`, …) that live on `tutti-plugin`'s `BridgeError`. The host crate
//! maps `PluginError` into its richer `BridgeError` at the IPC boundary.

use crate::editor::EditorError;
use crate::load_stage::LoadStage;

/// Failure produced by a plugin-instance capability trait method.
///
/// Deliberately narrow: only the failure modes a format loader can hit. The
/// `tutti-plugin` host crate widens this into its `BridgeError` (which adds
/// the IPC/subprocess variants) via `From<PluginError>`.
#[derive(Debug)]
pub enum PluginError {
    /// A plugin failed to load at a specific phase. `reason` carries the
    /// format-native error text.
    Load {
        /// Which phase of loading failed — the load path's own progress marker,
        /// so a caller can tell "the file was not found" from "the plugin
        /// refused to instantiate".
        stage: LoadStage,
        /// The format-native error text, verbatim.
        reason: String,
    },
    /// Saving or restoring plugin state failed.
    State(String),
    /// Saving or restoring state failed for a reason the host side knows
    /// exactly (the plugin crashed, the bridge was busy, the transfer stalled):
    /// the [`StateError`] a served plugin's bridge answered, kept whole so a
    /// caller reaching it through [`Control`](crate::Control) loses nothing.
    StateFailed(StateError),
    /// Processing an audio block failed.
    Process(String),
    /// A block failed for a reason that is a constant. Unlike
    /// [`Process`](Self::Process) it owns no heap memory, so the audio thread
    /// can build it for every errored block without allocating.
    Block(&'static str),
    /// Opening / resizing the plugin editor failed.
    Editor(EditorError),
    /// Any other failure that doesn't fit the categories above.
    Other(String),
}

impl core::fmt::Display for PluginError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Load { stage, reason } => {
                write!(f, "plugin load failed at {stage} stage: {reason}")
            }
            Self::State(msg) => write!(f, "plugin state error: {msg}"),
            Self::StateFailed(e) => write!(f, "plugin state error: {e}"),
            Self::Process(msg) => write!(f, "plugin process error: {msg}"),
            Self::Block(why) => write!(f, "plugin process error: {why}"),
            Self::Editor(e) => write!(f, "plugin editor error: {e}"),
            Self::Other(msg) => write!(f, "plugin error: {msg}"),
        }
    }
}

impl std::error::Error for PluginError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Editor(e) => Some(e),
            Self::StateFailed(e) => Some(e),
            _ => None,
        }
    }
}

impl From<EditorError> for PluginError {
    fn from(e: EditorError) -> Self {
        Self::Editor(e)
    }
}

/// Result alias for the plugin-instance capability trait methods.
pub type Result<T> = core::result::Result<T, PluginError>;

/// Whether a fire-and-forget command reached the plugin's queue.
///
/// Three states rather than a bool because two of them want **opposite**
/// responses: [`PluginDead`](Self::PluginDead) is permanent, so a caller should
/// stop trying, while [`Dropped`](Self::Dropped) is transient and the very next
/// call may succeed. A caller reading a merged `false` either spins against a
/// corpse or abandons a backlog that would have cleared.
///
/// Fieldless, and deliberately not a `Result`: there is no message to carry —
/// neither condition produces one — and these calls sit on the RT-adjacent
/// queue path where an allocation would not be welcome. A `Result` whose error
/// is a fieldless single variant implies a reason that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a command that was not delivered did not reach the plugin"]
pub enum Delivered {
    /// Queued for the audio thread.
    Yes,
    /// The queue was full. Transient — the same call may succeed next block.
    Dropped,
    /// The plugin is gone. Permanent; every later call answers the same.
    PluginDead,
}

impl Delivered {
    /// Returns `true` only for [`Yes`](Self::Yes), for a call site that wants
    /// a bool. Named rather than a `From` impl so the collapse is visible at the
    /// site that chooses it.
    #[must_use]
    pub fn is_delivered(self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// Why restoring a plugin's saved state failed.
///
/// Separate from [`PluginError::State`] rather than reusing it, because the two
/// answer different questions. `PluginError` is what a *loader* produces and
/// carries only what the plugin said; two of the three cases here are facts
/// about the **host side** — the plugin is gone, or this backend has no state
/// route at all — that no loader is in a position to report.
///
/// The distinction matters to a caller: [`Rejected`](Self::Rejected) means try a
/// different blob (the plugin was upgraded, the file is truncated, the chunk
/// came from another plugin), while [`PluginCrashed`](Self::PluginCrashed) means
/// stop using this plugin and [`NoStateRoute`](Self::NoStateRoute) means never
/// offer the operation again for this backend.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StateError {
    /// The plugin process is gone. Permanent — every later call answers the same.
    #[error("plugin subprocess has crashed")]
    PluginCrashed,

    /// The request could not be queued: the bridge's command queue is full,
    /// behind blocks it has not sent yet. The session is healthy and nothing
    /// was asked of the plugin, so a retry is reasonable. Reporting this as
    /// [`PluginCrashed`](Self::PluginCrashed) told a host to drop a working
    /// plugin.
    #[error("the plugin bridge is busy; the request was not sent")]
    Busy,

    /// The plugin was asked and refused, carrying its own message. Routine on a
    /// version bump: a plugin that changed its state format rejects a chunk
    /// saved by an older build of itself.
    #[error("plugin rejected the state: {0}")]
    Rejected(String),

    /// The plugin was not asked: its state is read on the host's main thread,
    /// and the main thread did not run the request within the deadline (it was
    /// not pumping, or was blocked on this very call). Nothing was refused and
    /// nothing was produced, so a caller must keep what it had, never
    /// substitute an empty state, and may retry.
    #[error("the main thread did not answer in time; the plugin was not asked")]
    Unavailable,

    /// This backend cannot carry state at all — distinct from a plugin that was
    /// asked and said no.
    #[error("this backend has no state route")]
    NoStateRoute,

    /// The state exceeded what the transport will carry, in either direction.
    ///
    /// A distinct variant rather than a [`Rejected`](Self::Rejected) string
    /// because the plugin did **not** reject anything: on save it produced a
    /// blob the host would not accept, and on load the host refused before the
    /// plugin was asked. Reporting either as a rejection blames the plugin for
    /// a limit the host imposes, and reporting it as
    /// [`PluginCrashed`](Self::PluginCrashed) — which is what an unbounded
    /// transport error degraded into — tells a DAW its plugin died when the
    /// session is fine.
    ///
    /// Both numbers are carried because only their *ratio* tells a user what to
    /// do: a blob a little over is a plugin to report upstream, one many times
    /// over is a corrupt file.
    #[error("plugin state is {bytes} bytes, over the {limit}-byte limit")]
    TooLarge {
        /// Size of the state that was refused.
        bytes: usize,
        /// The limit it exceeded.
        limit: usize,
    },

    /// The transfer stopped making progress: no further chunk arrived within
    /// the per-chunk deadline.
    ///
    /// **A progress deadline, not a total one, and the variant exists because
    /// the two are different diagnoses.** State travels as chunks, so the honest
    /// question is "is this transfer still moving?" — not "has it finished
    /// yet?". A fixed total budget answers the second, and answering the second
    /// makes the size limit unreachable: a large-but-legal state that streams
    /// steadily is failed for being big, while the message blames the plugin for
    /// hanging. A plugin that is slow but advancing is *working*, and must be
    /// allowed to finish however long it takes.
    ///
    /// Distinct from its three neighbours because each calls for a different
    /// response. [`TooLarge`](Self::TooLarge) is a size the host refused before
    /// or during transfer and will refuse again identically;
    /// [`Rejected`](Self::Rejected) is the plugin answering "no";
    /// [`PluginCrashed`](Self::PluginCrashed) means the subprocess is gone and
    /// every later call fails the same way. This is none of those — the socket
    /// is intact and the session is still healthy, so a retry is reasonable and
    /// the plugin may simply be wedged rather than dead.
    ///
    /// **Whoever produces this must leave the session alive.** The variant
    /// promises a healthy socket and a worthwhile retry, so a transport that
    /// reports a stall *and* tears the connection down is telling the caller two
    /// incompatible things — and the caller acts on the error it can see, not on
    /// the teardown it cannot.
    ///
    /// `bytes` is what had arrived when progress stopped, which is what
    /// separates "never started" (0) from "died four fifths of the way in".
    #[error(
        "plugin state transfer stalled after {bytes} bytes: no chunk arrived within {}ms",
        after.as_millis()
    )]
    Stalled {
        /// Bytes transferred before progress stopped. Zero means nothing ever
        /// arrived, which points at the plugin rather than the transport.
        bytes: usize,
        /// The per-chunk progress deadline that expired.
        after: std::time::Duration,
    },
}
