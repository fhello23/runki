pub mod app;
pub mod deck;
pub mod library;
pub mod progress;
pub mod session;

/// Runki's version, from `Cargo.toml` (the only place it is set).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
