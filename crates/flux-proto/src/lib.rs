//! Generated gRPC types for `flux.v1` plus conversions to [`flux_core`].

pub mod v1 {
    tonic::include_proto!("flux.v1");
}

mod convert;

pub use convert::*;

/// Max gRPC message size used by flux servers and clients.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// Payload size of one content frame.
pub const FRAME_BYTES: usize = 2 * 1024 * 1024;
