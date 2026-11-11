//! What a render writes, and where each output reads from (doc 019 section 4).
//!
//! One render pass feeds a list of [`Output`]s. An output is a [`Feed`] (the
//! audio it reads: the graph's outputs, or a plan tap) and a [`Target`] (what
//! it becomes: a file, or planes in memory). Every type has private fields, a
//! constructor and getters, and none has a default for a rate, a format or a
//! duration.

use std::path::{Path, PathBuf};

use tutti_codec::Format;
use tutti_dsp::DitherShape;
use tutti_graph::TapReader;
use tutti_types::{ChannelLayout, ChannelTopology, FoldError};

use crate::Resample;
use crate::normalize::Normalize;
use crate::{Rendered, Written};

/// Where the gain of a normalizing output comes from ([`NormalizeSpec`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GainSource {
    /// This output is measured, and its own reading chooses the gain.
    Own,
    /// The output at this index (in the list given to [`render`](crate::render()))
    /// is measured and this one takes its gain, so the two keep their
    /// balance (stems that still sum to the master). The index must name
    /// another output that normalizes by [`Own`](Self::Own).
    FromOutput(usize),
}

/// How a file output is normalized: a policy, and whose reading it follows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizeSpec {
    mode: Normalize,
    gain: GainSource,
}

impl NormalizeSpec {
    /// Measures this output and normalizes it by `mode`.
    #[must_use]
    pub fn own(mode: Normalize) -> Self {
        Self {
            mode,
            gain: GainSource::Own,
        }
    }

    /// Applies the gain measured on the output at `index`.
    ///
    /// `mode` is not consulted for the gain (the measured output's is); it is
    /// the policy this output would have been given on its own, and is kept so
    /// the spec states what the file was asked to be.
    #[must_use]
    pub fn from_output(mode: Normalize, index: usize) -> Self {
        Self {
            mode,
            gain: GainSource::FromOutput(index),
        }
    }

    /// The normalize policy.
    #[must_use]
    pub fn mode(&self) -> Normalize {
        self.mode
    }

    /// Whose reading the gain comes from.
    #[must_use]
    pub fn gain(&self) -> GainSource {
        self.gain
    }
}

/// A file one output is written to: where, in what format and width, and how
/// it is converted on the way (rate, dither, normalize).
///
/// A format and width the encoder cannot take, and a topology it cannot
/// place, are refused when the render starts (the file's rate is not known
/// until then), before any frame is rendered and before the path is touched.
#[derive(Debug, Clone, PartialEq)]
pub struct FileTarget {
    path: PathBuf,
    format: Format,
    channels: ChannelLayout,
    topology: Option<ChannelTopology>,
    resample: Option<Resample>,
    dither: Option<DitherShape>,
    normalize: Option<NormalizeSpec>,
}

impl FileTarget {
    /// A file at `path` of `format`, `channels` wide, at the render's rate,
    /// with triangular dither (it applies only to an integer width) and no
    /// normalize.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, format: Format, channels: ChannelLayout) -> Self {
        Self {
            path: path.into(),
            format,
            channels,
            topology: None,
            resample: None,
            dither: Some(DitherShape::Triangular),
            normalize: None,
        }
    }

    /// Which speaker each file channel feeds: for the WAV speaker mask, Ogg
    /// Vorbis's channel order and the fold onto the file. `None` is the
    /// engine's order for the width. Its width must be the file's.
    #[must_use]
    pub fn with_topology(mut self, topology: Option<ChannelTopology>) -> Self {
        self.topology = topology;
        self
    }

    /// Writes at another rate than the render's.
    #[must_use]
    pub fn with_resample(mut self, resample: Option<Resample>) -> Self {
        self.resample = resample;
        self
    }

    /// The noise added when an integer width is quantized; `None` adds none.
    /// A format with no integer width (`Pcm::Float32`, Ogg Vorbis) ignores it.
    #[must_use]
    pub fn with_dither(mut self, dither: Option<DitherShape>) -> Self {
        self.dither = dither;
        self
    }

    /// Normalizes this output ([`NormalizeSpec`]); `None` writes it as rendered.
    #[must_use]
    pub fn with_normalize(mut self, normalize: Option<NormalizeSpec>) -> Self {
        self.normalize = normalize;
        self
    }

    /// Where the file lands.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The container and its settings.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    /// The file's width.
    #[must_use]
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// The speakers the file's channels feed, if stated.
    #[must_use]
    pub fn topology(&self) -> Option<&ChannelTopology> {
        self.topology.as_ref()
    }

    /// The rate conversion on the way out, if any.
    #[must_use]
    pub fn resample(&self) -> Option<Resample> {
        self.resample
    }

    /// The dither, if any.
    #[must_use]
    pub fn dither(&self) -> Option<DitherShape> {
        self.dither
    }

    /// The normalize, if any.
    #[must_use]
    pub fn normalize(&self) -> Option<NormalizeSpec> {
        self.normalize
    }
}

/// Planes in memory: an output rendered at the render's rate, not resampled
/// and not dithered (they are not an output a listener hears), returned as a
/// [`Rendered`].
#[derive(Debug, Clone, PartialEq)]
pub struct BufferTarget {
    channels: ChannelLayout,
    topology: Option<ChannelTopology>,
}

impl BufferTarget {
    /// Planes `channels` wide.
    #[must_use]
    pub fn new(channels: ChannelLayout) -> Self {
        Self {
            channels,
            topology: None,
        }
    }

    /// Which speakers the planes are, for the fold onto them; `None` is the
    /// engine's order for the width. Its width must be the planes'.
    #[must_use]
    pub fn with_topology(mut self, topology: Option<ChannelTopology>) -> Self {
        self.topology = topology;
        self
    }

    /// The planes' width.
    #[must_use]
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// The speakers the planes are, if stated.
    #[must_use]
    pub fn topology(&self) -> Option<&ChannelTopology> {
        self.topology.as_ref()
    }
}

/// What an output becomes.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Target {
    /// A file.
    File(FileTarget),
    /// Planes in memory.
    Buffers(BufferTarget),
}

impl From<FileTarget> for Target {
    fn from(target: FileTarget) -> Self {
        Self::File(target)
    }
}

impl From<BufferTarget> for Target {
    fn from(target: BufferTarget) -> Self {
        Self::Buffers(target)
    }
}

/// What the channels of a tap are: the speakers (or unassigned feeds) its lanes
/// carry, in lane order, so the fold onto the target's layout can place them.
///
/// Built by whoever opened the tap (the session, for a stem); the pipeline
/// checks it against the tap's width and the target's layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    topology: ChannelTopology,
}

impl Placement {
    /// Lanes that are the channels of `topology`.
    #[must_use]
    pub fn new(topology: ChannelTopology) -> Self {
        Self { topology }
    }

    /// What the lanes are.
    #[must_use]
    pub fn topology(&self) -> &ChannelTopology {
        &self.topology
    }
}

/// The audio an output reads.
#[derive(Debug)]
#[non_exhaustive]
pub enum Feed {
    /// The graph's outputs (the declared root, or the engine's order for the
    /// width), folded onto the target's layout.
    Master,
    /// A plan tap ([`Editor::open_tap`](tutti_graph::Editor::open_tap)), read
    /// after every block: its lanes are `lanes`, folded onto the target's
    /// layout.
    ///
    /// Open the tap with [`TapAlign::ToOutput`](tutti_graph::TapAlign::ToOutput)
    /// so it is on the outputs' time (one latency trim serves every output),
    /// give each lane `2 x max_block` frames of ring, and commit before the
    /// render. A lane count that is not `lanes`' width is
    /// [`TargetError::Lanes`]; frames the ring lost are
    /// [`Error::TapOverrun`](crate::Error::TapOverrun).
    Tap {
        /// The tap's reader: the render is its one consumer.
        reader: TapReader,
        /// What its lanes are.
        lanes: Placement,
    },
}

/// One thing a render writes: the [`Feed`] it reads and the [`Target`] it
/// becomes.
#[derive(Debug)]
pub struct Output {
    feed: Feed,
    target: Target,
}

impl Output {
    /// The graph's outputs, into `target`.
    #[must_use]
    pub fn master(target: impl Into<Target>) -> Self {
        Self {
            feed: Feed::Master,
            target: target.into(),
        }
    }

    /// What `reader` taps, placed as `lanes`, into `target`.
    #[must_use]
    pub fn tap(reader: TapReader, lanes: Placement, target: impl Into<Target>) -> Self {
        Self {
            feed: Feed::Tap { reader, lanes },
            target: target.into(),
        }
    }

    /// What this output reads.
    #[must_use]
    pub fn feed(&self) -> &Feed {
        &self.feed
    }

    /// What this output becomes.
    #[must_use]
    pub fn target(&self) -> &Target {
        &self.target
    }

    pub(crate) fn into_parts(self) -> (Feed, Target) {
        (self.feed, self.target)
    }
}

/// What one [`Output`] produced, in the order the outputs were given.
#[derive(Debug)]
#[non_exhaustive]
pub enum Produced {
    /// From a [`Target::File`].
    File(Written),
    /// From a [`Target::Buffers`].
    Buffers(Rendered),
}

impl Produced {
    /// The file, or `None` for planes.
    #[must_use]
    pub fn file(self) -> Option<Written> {
        match self {
            Self::File(written) => Some(written),
            Self::Buffers(_) => None,
        }
    }

    /// The planes, or `None` for a file.
    #[must_use]
    pub fn buffers(self) -> Option<Rendered> {
        match self {
            Self::Buffers(rendered) => Some(rendered),
            Self::File(_) => None,
        }
    }
}

/// Why an [`Output`] cannot be rendered ([`Error::Target`](crate::Error::Target)).
#[derive(thiserror::Error, Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TargetError {
    /// The channels (the graph's, or a tap's lanes) have no place in the
    /// target's layout: the topology it states cannot take them, or is not as
    /// wide as the target.
    #[error("the channels cannot be placed into the target's: {0}")]
    Placement(FoldError),
    /// The tap has a lane count other than its [`Placement`]'s width.
    #[error("the tap carries {lanes:?} but its placement names {placement:?}")]
    Lanes {
        /// The tap's width.
        lanes: ChannelLayout,
        /// The placement's width.
        placement: ChannelLayout,
    },
    /// A [`GainSource::FromOutput`] that names this output, an output past
    /// the list, or one that does not measure itself.
    #[error(
        "the gain is to come from output {index}, which is not another output that measures itself"
    )]
    GainSource {
        /// The index it names.
        index: usize,
    },
    /// [`write_buffers`](crate::write_buffers) was given a target that asks to
    /// normalize: planes are written as they are, so the gain is applied first
    /// ([`Rendered::apply_gain`](crate::Rendered::apply_gain)).
    #[error("planes are written as they are: apply the gain first, and ask no normalize")]
    NormalizeOfPlanes,
    /// Another output writes the same path.
    #[error("output {other} writes the same path")]
    SamePath {
        /// The other output.
        other: usize,
    },
}
