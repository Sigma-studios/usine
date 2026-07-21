//! Pure domain: the data model, its persisted configuration types, and the pure
//! card state machine. No I/O — no storage, provider, or git dependencies.

pub mod config;
pub mod model;
pub mod state_machine;
