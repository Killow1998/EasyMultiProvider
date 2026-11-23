mod api;
mod app;
mod cli;
mod error;
mod http;
mod lifecycle;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "The server runtime is being integrated in stages")
)]
mod server;
mod services;
mod startup_recovery;
mod tray;
mod util;
mod web;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub use web::WEB_INDEX_BYTES;

pub fn run() -> Result<std::process::ExitCode, String> {
    cli::run()
}

#[cfg(test)]
mod tests;
