#[cfg(feature = "gui")]
pub mod app;
pub mod config;
pub mod deck;
pub mod library;
pub mod progress;
pub mod session;
#[cfg(feature = "gui")]
pub mod theme;
pub mod tui;
pub mod web;
#[cfg(feature = "gui")]
pub mod web_menu;

/// Runki's version, from `Cargo.toml` (the only place it is set).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
