//! Compile-time generated Burn models from ONNX.

#[cfg(feature = "onnx-models")]
pub mod tiny_effect {
    include!(concat!(env!("OUT_DIR"), "/model/tiny_effect.rs"));
}
