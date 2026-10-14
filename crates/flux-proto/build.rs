fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = "../../proto";
    // Use a vendored protoc unless the caller provides one.
    if std::env::var_os("PROTOC").is_none() {
        // SAFETY: build scripts are single-threaded at this point.
        unsafe { std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?) };
    }
    println!("cargo:rerun-if-changed={proto_root}");
    tonic_prost_build::configure()
        // Zero-copy payloads for the hot object paths.
        .bytes(".flux.v1.ContentFrame.data")
        .bytes(".flux.v1.UploadFrame.data")
        .bytes(".flux.v1.TreeObject.canonical")
        .compile_protos(&[format!("{proto_root}/flux/v1/flux.proto")], &[proto_root.to_string()])?;
    Ok(())
}
