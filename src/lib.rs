//! The library half of the binary. Everything the server does lives here so the integration
//! tests drive the same code the app runs.

pub mod auth;
pub mod cache;
pub mod config;
pub mod db;
pub mod error;
pub mod media;
pub mod metrics;
pub mod proxy;
pub mod source;
pub mod state;
pub mod time;
pub mod util;
pub mod web;
