//! The in-memory SoundFont builder (`tutti_fixtures::sf2`) with the parsing step.

#![allow(dead_code)] // each test binary uses its own subset

use std::sync::Arc;

use tutti_sf2::SoundFont;

pub use tutti_fixtures::sf2::{Inst, Preset, Zone};

/// The file, parsed.
pub fn font(presets: &[Preset], insts: &[Inst]) -> Arc<SoundFont> {
    Arc::new(
        SoundFont::new(&mut std::io::Cursor::new(tutti_fixtures::sf2::bytes(
            presets, insts,
        )))
        .expect("the built SoundFont parses"),
    )
}

/// One preset (bank 0, program 0) with one instrument of the given zones.
pub fn one_preset(zones: Vec<Zone>) -> Arc<SoundFont> {
    font(
        &[Preset {
            name: "p",
            bank: 0,
            program: 0,
            zones: vec![(vec![], 0)],
        }],
        &[Inst { name: "i", zones }],
    )
}
