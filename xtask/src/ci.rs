use crate::{Result, project_root};
use regex::Regex;
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

pub enum Packages {
    Selected(&'static [&'static str]),
    Workspace { no_fail_fast: bool },
}

pub enum Targets {
    Default,
    Lib,
    Tests(&'static [&'static str]),
    All,
}

enum LibtestArg {
    Ignored,
    NoCapture,
    TestThreads(usize),
    Skip(&'static str),
}

pub struct Invocation {
    pub packages: Packages,
    pub targets: Targets,
    filter: Option<&'static str>,
    libtest: &'static [LibtestArg],
}

pub struct Profile {
    pub name: &'static str,
    pub invocations: &'static [Invocation],
}

pub const PROFILES: &[Profile] = &[
    Profile {
        name: "native-tls",
        invocations: &[Invocation {
            filter: Some("tls_rejects_untrusted_and_wrong_host_but_accepts_a_trusted_name"),
            ..Invocation::new(
                Packages::Selected(&["emp-transport"]),
                Targets::Tests(&["http_client"]),
            )
        }],
    },
    Profile {
        name: "native-memory",
        invocations: &[Invocation {
            libtest: &[LibtestArg::NoCapture],
            ..Invocation::lib(&["emp-transport"], "system_memory")
        }],
    },
    Profile {
        name: "large-http-admission",
        invocations: &[Invocation {
            libtest: &[LibtestArg::Ignored, LibtestArg::NoCapture],
            ..Invocation::lib(&["emp-app"], "native_memory_large_http_body")
        }],
    },
    Profile {
        name: "startup-recovery-and-exit",
        invocations: &[Invocation {
            libtest: &[LibtestArg::TestThreads(1)],
            ..Invocation::new(
                Packages::Selected(&["emp-app"]),
                Targets::Tests(&[
                    "desktop_startup",
                    "startup_recovery",
                    "exit_process",
                    "http_support",
                ]),
            )
        }],
    },
    Profile {
        name: "tray-actions",
        invocations: &[Invocation {
            libtest: &[LibtestArg::TestThreads(1)],
            ..Invocation::lib(&["emp-app"], "tray::actions::tests")
        }],
    },
    Profile {
        name: "disconnect",
        invocations: &[Invocation {
            libtest: &[LibtestArg::TestThreads(1)],
            ..Invocation::lib(&["emp-app"], "disconnect")
        }],
    },
    Profile {
        name: "websocket-native-io",
        invocations: &[
            Invocation::new(
                Packages::Selected(&["emp-transport"]),
                Targets::Tests(&["websocket_async"]),
            ),
            Invocation::lib(
                &["emp-transport"],
                "async_reads_keep_partial_frames_and_allow_concurrent_large_write",
            ),
        ],
    },
    Profile {
        name: "windows-process-handoff",
        invocations: &[
            Invocation::lib(&["emp-updater"], "process::tests"),
            Invocation::lib(
                &["emp-app"],
                "configured_browser_helpers_never_allocate_a_console",
            ),
            Invocation::lib(
                &["emp-transport"],
                "background_proxy_commands_never_allocate_a_console",
            ),
            Invocation {
                libtest: &[LibtestArg::Skip("detects_directory_reparse_points")],
                ..Invocation::lib(&["emp-files"], "private_windows::tests")
            },
            Invocation::lib(&["emp-storage"], "vault::store::windows_tests"),
        ],
    },
    Profile {
        name: "workspace",
        invocations: &[Invocation::new(
            Packages::Workspace { no_fail_fast: true },
            Targets::All,
        )],
    },
    Profile {
        name: "windows-private-state",
        invocations: &[Invocation::new(
            Packages::Selected(&["emp-files", "emp-storage"]),
            Targets::Default,
        )],
    },
    Profile {
        name: "windows-shutdown",
        invocations: &[Invocation::lib(&["emp-app"], "lifecycle::shutdown::tests")],
    },
    Profile {
        name: "windows-exit-process",
        invocations: &[Invocation::new(
            Packages::Selected(&["emp-app"]),
            Targets::Tests(&["exit_process", "http_support"]),
        )],
    },
    Profile {
        name: "macos-discovery",
        invocations: &[
            Invocation::lib(&["emp-codex"], "runtime_inventory::"),
            Invocation::lib(&["emp-codex"], "executable_trust::"),
            Invocation::lib(&["emp-app"], "browser_login_classifies_engine_failures"),
            Invocation::lib(&["emp-app"], "tray::actions::tests"),
            Invocation::new(
                Packages::Selected(&["emp-app"]),
                Targets::Tests(&["desktop_startup", "exit_process", "http_support"]),
            ),
        ],
    },
    Profile {
        name: "macos-installed-cli",
        invocations: &[Invocation {
            libtest: &[LibtestArg::Ignored, LibtestArg::NoCapture],
            ..Invocation::lib(&["emp-codex"], "installed_macos_cli_discovery")
        }],
    },
    Profile {
        name: "macos-installed-app",
        invocations: &[Invocation {
            libtest: &[LibtestArg::Ignored, LibtestArg::NoCapture],
            ..Invocation::lib(&["emp-codex"], "installed_macos_")
        }],
    },
];

impl Invocation {
    pub const fn new(packages: Packages, targets: Targets) -> Self {
        Self {
            packages,
            targets,
            filter: None,
            libtest: &[],
        }
    }

    const fn lib(packages: &'static [&'static str], filter: &'static str) -> Self {
        Self {
            filter: Some(filter),
            ..Self::new(Packages::Selected(packages), Targets::Lib)
        }
    }

    fn args(&self) -> Vec<String> {
        let mut args = vec!["test".into(), "--locked".into()];
        match self.packages {
            Packages::Selected(names) => {
                for &name in names {
                    args.extend(["-p".into(), name.into()]);
                }
            }
            Packages::Workspace { .. } => args.push("--workspace".into()),
        }
        match self.targets {
            Targets::Default => {}
            Targets::Lib => args.push("--lib".into()),
            Targets::Tests(names) => {
                for &name in names {
                    args.extend(["--test".into(), name.into()]);
                }
            }
            Targets::All => args.push("--all-targets".into()),
        }
        if let Packages::Workspace { no_fail_fast: true } = self.packages {
            args.push("--no-fail-fast".into());
        }
        if let Some(filter) = self.filter {
            args.push(filter.into());
        }
        if !self.libtest.is_empty() {
            args.push("--".into());
            for arg in self.libtest {
                match arg {
                    LibtestArg::Ignored => args.push("--ignored".into()),
                    LibtestArg::NoCapture => args.push("--nocapture".into()),
                    LibtestArg::TestThreads(count) => args.push(format!("--test-threads={count}")),
                    LibtestArg::Skip(name) => args.extend(["--skip".into(), (*name).into()]),
                }
            }
        }
        args
    }
}

pub fn run(args: &[String]) -> Result {
    let [name] = args else {
        return Err("usage: cargo xtask ci <profile>|--list".into());
    };
    if name == "--list" {
        for profile in PROFILES {
            println!("{}", profile.name);
            for invocation in profile.invocations {
                println!("  cargo {}", invocation.args().join(" "));
            }
        }
        return Ok(());
    }
    let profile = PROFILES
        .iter()
        .find(|profile| profile.name == name)
        .ok_or_else(|| format!("unknown CI profile {name:?}; run cargo xtask ci --list"))?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let root = project_root();
    for invocation in profile.invocations {
        let mut command = Command::new(&cargo);
        command
            .current_dir(&root)
            .args(invocation.args())
            .stdout(Stdio::piped());
        let description = format!("ci {}: {command:?}", profile.name);
        println!("{description}");
        let mut child = command
            .spawn()
            .map_err(|error| format!("{description}: {error}"))?;
        let passed = forward_test_output(
            child.stdout.take().expect("piped cargo stdout"),
            io::stdout().lock(),
        );
        let status = child
            .wait()
            .map_err(|error| format!("{description}: {error}"))?;
        if !status.success() {
            return Err(format!("{description}: {status}"));
        }
        if passed.map_err(|error| format!("{description}: cargo stdout: {error}"))? == 0 {
            return Err(format!("{description}: 过滤条件或目标没有匹配到任何测试"));
        }
    }
    Ok(())
}

fn forward_test_output(mut output: impl Read, mut stdout: impl Write) -> io::Result<usize> {
    let mut buffer = [0; 8192];
    let mut passed = 0;
    let mut line = Vec::new();
    loop {
        let read = match output.read(&mut buffer) {
            Ok(0) => return Ok(count_passed(passed, &line)),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let chunk = &buffer[..read];
        stdout.write_all(chunk)?;
        stdout.flush()?;

        for part in chunk.split_inclusive(|&byte| byte == b'\n') {
            if let Some(end) = part.strip_suffix(b"\n") {
                if line.is_empty() {
                    passed = count_passed(passed, end);
                } else {
                    line.extend_from_slice(end);
                    passed = count_passed(passed, &line);
                    line.clear();
                }
            } else {
                line.extend_from_slice(part);
            }
        }
    }
}

fn count_passed(passed: usize, line: &[u8]) -> usize {
    static SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^test result: (?:ok|FAILED)\. ([0-9]+) passed; [0-9]+ failed; [0-9]+ ignored\b",
        )
        .expect("libtest summary pattern")
    });
    let count = std::str::from_utf8(line)
        .ok()
        .and_then(|line| SUMMARY.captures(line))
        .and_then(|summary| summary[1].parse::<usize>().ok());
    passed + count.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_a_summary_split_across_reads() {
        let first = b"running 2 tests\ntest result: ok. 2 ";
        let second = b"passed; 0 failed; 0 ignored\n";
        let mut forwarded = Vec::new();
        let passed = forward_test_output(first.as_slice().chain(second.as_slice()), &mut forwarded);
        assert_eq!(passed.unwrap(), 2);
        assert_eq!(forwarded, [first.as_slice(), second.as_slice()].concat());
    }

    #[test]
    fn sums_passed_tests_across_binaries() {
        let passed = count_passed(
            0,
            b"test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured\n",
        );
        assert_eq!(
            count_passed(
                passed,
                b"test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured\r\n"
            ),
            5
        );
    }

    #[test]
    fn counts_only_passed_tests() {
        assert_eq!(
            count_passed(
                0,
                b"test result: ok. 0 passed; 0 failed; 4 ignored; 0 measured; 7 filtered out"
            ),
            0
        );
    }

    #[test]
    fn ignores_other_cargo_and_test_output() {
        let lines = [
            "running 2 tests",
            "log: test result: ok. 99 passed; 0 failed; 0 ignored",
            "test result: ok. 51 passed; still running",
            "test example ... FAILED",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured",
            "error: test failed, to rerun pass `-p example --lib`",
        ];
        let passed = lines
            .iter()
            .fold(0, |total, line| count_passed(total, line.as_bytes()));
        assert_eq!(passed, 1);
    }
}
