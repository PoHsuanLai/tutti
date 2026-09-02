//! Deliberately empty.
//!
//! This crate exists only to carry one `loom` model, in
//! `tests/shm_header_ordering.rs`. It has no library surface and nothing
//! depends on it — see `Cargo.toml` for why the model cannot live in
//! `tutti-plugin`, where the code it models does.
