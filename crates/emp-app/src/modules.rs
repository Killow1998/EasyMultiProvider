#[allow(
    dead_code,
    reason = "hooks are wired when their first module consumer migrates"
)]
pub(crate) mod hooks;
pub mod process;
pub mod registry;
