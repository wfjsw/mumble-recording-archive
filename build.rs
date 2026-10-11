fn main() {
    println!("cargo:rerun-if-changed=proto/Mumble.proto");
    prost_build::Config::new()
        .compile_protos(&["proto/Mumble.proto"], &["proto/"])
        .expect("compile Mumble.proto (protoc must be on PATH or set PROTOC)");
}
