pub mod activity;
pub mod admission;
pub mod agent_proc;
pub mod board;
pub mod bot;
pub mod channel;
pub mod cli;
pub mod client;
pub mod compat;
pub mod config;
pub mod db;
pub mod dispatch;
pub mod flow;
pub mod goal;
pub mod goal_board;
pub mod housekeeping;
pub mod run_log;
pub mod schedule;
pub mod server;
pub mod subscription;
#[cfg(feature = "telegram")]
pub mod telegram;
pub mod usage;
#[cfg(feature = "embed-ui")]
pub mod web;
pub mod worktree;

pub mod remote;

pub mod routine;

pub mod metrics;
