#![cfg(unix)]

mod support;

mod e2e {
    mod codex_config;
    mod forwarding;
    mod management;
    #[cfg(target_os = "linux")]
    mod native;
    mod streams;
    mod usage;
}
