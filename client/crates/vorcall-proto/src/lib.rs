//! Rust bindings for `proto/vorcall.proto`, generated at build time by protox +
//! prost. Never hand-edit the generated code; change the schema instead.

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/vorcall.v1.rs"));
}

pub use prost::Message;
