//! LINE adapter.
//!
//! This adapter owns LINE-specific communication, webhook verification, and
//! conversion from LINE webhook payloads into adapter-neutral events.

pub mod client;
pub mod types;
pub mod webhook;
