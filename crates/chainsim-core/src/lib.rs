//! chainsim core: the Adapter trait, the fork object, the JSON envelope, the
//! error codes, profiles, the engine lock, and Docker control through the
//! control sidecar.

pub mod adapter;
pub mod control;
pub mod docker;
pub mod engines;
pub mod envelope;
pub mod error;
pub mod fork;
pub mod lock;
pub mod profile;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
}

pub use adapter::Adapter;
pub use error::{Error, ErrorCode, Result};
