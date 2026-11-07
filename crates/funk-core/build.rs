fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    // SAFETY: build scripts are single-threaded at this point.
    unsafe { std::env::set_var("PROTOC", protoc) };
    prost_build::compile_protos(&["proto/Mumble.proto", "proto/MumbleUDP.proto"], &["proto"])
        .expect("compile protos");
    println!("cargo:rerun-if-changed=proto");
}
