//! Platform-independent core of the Funk client: Mumble control protocol,
//! server state mirror and (later) voice handling. No UI or OS dependencies.

pub mod client;
pub mod state;
pub mod voice;
pub mod wire;

/// Generated protobuf types for the Mumble control and UDP protocols.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/mumble_proto.rs"));
    pub mod udp {
        include!(concat!(env!("OUT_DIR"), "/mumble_udp.rs"));
    }
}

pub use client::{Client, Config, Error, Event, connect};
