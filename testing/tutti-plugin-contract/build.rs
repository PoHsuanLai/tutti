//! Forwards the C++ plugin's bundle directory (`DEP_TUTTI_SHARED_VST3_CPP_DIR`,
//! set by its `links`) to the crate as `SHARED_VST3_CPP_DIR`.

fn main() {
    let dir = std::env::var("DEP_TUTTI_SHARED_VST3_CPP_DIR")
        .expect("tutti-shared-plugin-vst3-cpp builds the C++ plugin and sets its directory");
    println!("cargo:rustc-env=SHARED_VST3_CPP_DIR={dir}");
}
