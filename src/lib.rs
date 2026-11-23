//! Edition: release lines, tags, channels and an append-only deploy ledger.
//!
//! Edition orchestrates existing tools. It never signs, uploads or reads secrets
//! itself; it decides whether a ref may go to a channel, prints the exact commands,
//! runs them only when asked, and records what happened.

pub mod cli;
pub mod commands;
pub mod concordance;
pub mod config;
pub mod content;
pub mod git;
pub mod ledger;
pub mod ledger_store;
pub mod pending;
pub mod policy;
pub mod steps;
pub mod util;
pub mod verify;
pub mod version;
