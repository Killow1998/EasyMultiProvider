mod api;
mod app;
mod cli;
mod error;
mod http;
mod lifecycle;
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

#[cfg(all(test, unix))]
#[path = "../tests/support/executable.rs"]
mod test_executable;
#[cfg(test)]
#[path = "../tests/support/time.rs"]
mod test_time;
#[cfg(test)]
mod tests;
