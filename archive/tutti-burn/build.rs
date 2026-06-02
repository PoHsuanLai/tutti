fn main() {
    #[cfg(feature = "onnx-models")]
    {
        use burn_import::onnx::ModelGen;

        ModelGen::new()
            .input("src/model/tiny_effect.onnx")
            .out_dir("model/")
            .embed_states(true)
            .run_from_script();
    }
}
