//! The agent execution engine: the provider abstraction, the command/event
//! protocol, the executor that drives runs, the agent-I/O modules that define the
//! prompt-and-parse protocol (plan, review, commit, handoff), and startup
//! reconciliation.

pub mod block;
pub mod commit;
pub mod events;
pub mod executor;
pub mod handoff;
pub mod investigate;
pub mod plan;
pub mod provider;
pub mod review;
pub mod startup;
pub mod testing;
pub mod usage;
