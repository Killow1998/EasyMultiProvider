//! EMP application composition and consumer interfaces.
mod api;
mod app;
mod cli;
mod error;
mod http;
mod lifecycle;
mod services;
mod util;
mod web;

pub const VERSION: &str = "0.11.6";
pub use web::WEB_INDEX_BYTES;

pub fn run() -> Result<(), String> {
    cli::run()
}

#[cfg(test)]
mod tests;
