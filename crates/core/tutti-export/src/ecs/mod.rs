//! Bevy ECS surface for the offline export pipeline.
//!
//! - [`export`] — `StartExport` message → file render ([`TuttiExportPlugin`]).
//!   Unconditional; depends only on tutti-core's graph + task hub.
//! - [`render_region`] — offline per-node region render
//!   ([`TuttiRegionRenderPlugin`]). Gated on the `sampler` feature: it isolates
//!   and rebinds clip-reader / sampler units from `tutti-sampler`.

pub mod export;
pub use export::{
    export_poll_system, export_start_system, ExportComplete, ExportFailed, ExportInProgress,
    StartExport, TuttiExportPlugin,
};

#[cfg(feature = "sampler")]
pub mod render_region;
#[cfg(feature = "sampler")]
pub use render_region::{
    prepare_region_render_system, region_render_poll_system, spawn_region_render_system,
    RegionRenderComplete, RegionRenderConfig, RegionRenderFailed, RegionRenderInProgress,
    RegionRenderNet, RegionRenderSystems, StartRegionRender, TuttiRegionRenderPlugin,
};
