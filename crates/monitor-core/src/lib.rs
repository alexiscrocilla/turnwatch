//! Reads Claude Code session state from local files and builds the snapshot
//! shown by the monitor window. No network access except `gh` for PR status.

pub mod aggregate;
pub mod desktop;
pub mod engine;
pub mod git_pr;
pub mod model;
pub mod paths;
pub mod registry;
pub mod transcript;
pub mod usage;
