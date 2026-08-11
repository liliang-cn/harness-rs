//! Compiles CortexDB's `MemoryService` into a tonic client — but only when the
//! `grpc` feature is on, so a default build needs neither `tonic-build` nor
//! `protoc`.
//!
//! The `.proto` files are vendored under `proto/` rather than read from a
//! CortexDB checkout: a published crate has to build from its own contents.

fn main() {
    #[cfg(feature = "grpc")]
    {
        println!("cargo:rerun-if-changed=proto/cortexdb/v1/memory.proto");
        println!("cargo:rerun-if-changed=proto/cortexdb/v1/common.proto");
        tonic_build::configure()
            .build_server(false) // a client is all a Memory needs
            .compile_protos(&["proto/cortexdb/v1/memory.proto"], &["proto"])
            .expect("failed to compile CortexDB memory protos");
    }
}
