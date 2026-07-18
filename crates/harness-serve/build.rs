//! Compiles `proto/chat.proto` into tonic server/client stubs — but only when
//! the `grpc` feature is on, so a default build needs neither `tonic-build` nor
//! `protoc`.

fn main() {
    #[cfg(feature = "grpc")]
    {
        println!("cargo:rerun-if-changed=proto/chat.proto");
        tonic_build::compile_protos("proto/chat.proto")
            .expect("failed to compile proto/chat.proto");
    }
}
