//! Agyva: account manager for Google's Antigravity CLI (`agy`).
//!
//! The binary in `main.rs` is a thin shell over this library so that every
//! behavior (including crash recovery) can be exercised from tests.

// Windows ACL and file-index calls need unsafe. Everywhere else stays forbidden.
#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(windows, deny(unsafe_code))]

pub mod agy;
pub mod cli;
pub mod config;
pub mod core;
pub mod notify;
pub mod paths;
pub mod storage;
#[doc(hidden)]
pub mod testing;
pub mod tui;
pub mod updater;
