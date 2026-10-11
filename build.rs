fn main() {
    println!("cargo:rerun-if-changed=proto/Mumble.proto");
    println!("cargo:rerun-if-env-changed=PROTOC");
    let mut config = prost_build::Config::new();
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
        config.protoc_executable(protoc);
    }
    config
        .compile_protos(&["proto/Mumble.proto"], &["proto/"])
        .expect("compile Mumble.proto");
}
