//! Bevy ECS surface for the offline export pipeline.
//!
//! - [`render_region`] — offline per-node region render
//!   ([`TuttiRegionRenderPlugin`]). Gated on the `sampler` feature: it isolates
//!   and rebinds clip-reader / sampler units from `tutti-sampler`.
//!
//! (The message-driven whole-graph `StartExport` pipeline was removed as dead
//! scaffolding — nothing ever sent the message. Offline whole-graph export is
//! driven directly through the [`GraphExport`](crate::GraphExport) builder, as
//! `dawai-frontend`'s project export does.)

#[cfg(feature = "sampler")]
pub mod render_region;
#[cfg(feature = "sampler")]
pub use render_region::{
    prepare_region_render_system, region_render_poll_system, spawn_region_render_system,
    RegionRenderComplete, RegionRenderConfig, RegionRenderFailed, RegionRenderInProgress,
    RegionRenderNet, RegionRenderSystems, StartRegionRender, TuttiRegionRenderPlugin,
};
