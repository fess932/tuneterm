fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The vendored protoc, so the build needs nothing installed beyond cargo.
    // SAFETY: a build script is single-threaded; nothing else reads the environment.
    unsafe {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }
    tonic_prost_build::configure()
        .compile_protos(&["proto/tuneterm/v1/library.proto"], &["proto"])?;
    Ok(())
}
