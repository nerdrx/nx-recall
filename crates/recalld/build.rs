fn main() {
    // sherpa-rs links libsherpa-onnx-c-api.so and libonnxruntime.so, which its
    // build script drops beside the built binary. Cargo exports a library path
    // for `cargo run`/`cargo test`, but an installed binary is on its own — so
    // bake in a search path relative to the executable and ship the .so files
    // next to it (or one directory up, in `lib/`).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib");
    }
}
