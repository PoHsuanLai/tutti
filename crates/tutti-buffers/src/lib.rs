#![doc = include_str!("../README.md")]
//! Audio sample formats and the borrowed views over them.
//!
//! A [`Sample`] is a storage format: `i8`, `i16`, [`I24`], `i32`, `f32` or
//! `f64`. A [`Float`] is a `Sample` that arithmetic runs in (`f32` or `f64`),
//! with its math in `libm`. The two never merge: a format says what a buffer
//! holds, a float says what a node computes in.
//!
//! The views borrow a slice and allocate nothing; each is generic over the
//! format, defaulting to `f32`:
//!
//! - [`Mono`] / [`MonoMut`]: one channel;
//! - [`Interleaved`] / [`InterleavedMut`]: whole frames in one slice, carrying
//!   their [`ChannelLayout`](tutti_types::ChannelLayout);
//! - [`Planes`] / [`PlanesMut`]: one equal-length slice per channel.
//!
//! Construction checks once and returns a [`BufferError`] for a zero width, a
//! length that is not whole frames or a ragged plane; nothing truncates. Every
//! count is [`Samples`](tutti_types::Samples), in frames.
//!
//! [`convert()`] changes format, [`interleave`] and [`deinterleave`] change
//! layout; a mismatch is an error, never a short copy.
//!
//! ```
//! use tutti_buffers::{convert, deinterleave, Interleaved, InterleavedMut, PlanesMut};
//! use tutti_types::ChannelLayout;
//!
//! let pcm = [16384i16, -16384, 8192, 0]; // two stereo frames
//! let mut audio = [0.0f32; 4];
//! convert(
//!     Interleaved::new(&pcm, ChannelLayout::STEREO)?,
//!     InterleavedMut::new(&mut audio, ChannelLayout::STEREO)?,
//! )?;
//! assert_eq!(audio, [0.5, -0.5, 0.25, 0.0]);
//!
//! let (mut l, mut r) = ([0.0f32; 2], [0.0f32; 2]);
//! let mut planes = [&mut l[..], &mut r[..]];
//! deinterleave(
//!     Interleaved::new(&audio, ChannelLayout::STEREO)?,
//!     PlanesMut::new(&mut planes)?,
//! )?;
//! assert_eq!((l, r), ([0.5, 0.25], [-0.5, 0.0]));
//! # Ok::<(), tutti_buffers::BufferError>(())
//! ```
#![forbid(unsafe_code)]

mod convert;
mod error;
mod i24;
mod interleaved;
mod mono;
mod planes;
mod sample;

pub use convert::{convert, deinterleave, interleave};
pub use error::BufferError;
pub use i24::{I24, I24OutOfRange};
pub use interleaved::{Interleaved, InterleavedMut};
pub use mono::{Mono, MonoMut};
pub use planes::{Planes, PlanesMut};
pub use sample::{Float, Sample};
