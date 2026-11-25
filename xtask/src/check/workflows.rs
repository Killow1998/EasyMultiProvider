use regex::Regex;
use std::{collections::BTreeSet, sync::LazyLock};

const ALLOWED: &[(&str, &str)] = &[(
    ".github/workflows/module-e2e.yml",
    "M04 验收真实 Aero 包；Python 仅用于安装 PyInstaller 和打包，场景由 Rust 执行",
)];

pub(super) fn check(path: &str, contents: &str, errors: &mut Vec<String>) {
    if ALLOWED.iter().any(|(file, _reason)| *file == path) {
        return;
    }
    for (index, line) in contents.lines().enumerate() {
        let lower = line.to_ascii_lowercase();
        let python = lower
            .split(|c: char| !c.is_ascii_alphanumeric() && !"._".contains(c))
            .any(|token| {
                token.ends_with(".py")
                    || token.split('.').any(|word| {
                        let command = word.trim_end_matches(|c: char| c.is_ascii_digit());
                        matches!(command, "python" | "pip")
                    })
            });
        if python {
            errors.push(format!("[no-python-on-main] {path}:{}: main 的 CI 不能依赖 Python；如果这个 workflow 只在发版/手动时运行，把它加入清单并说明", index + 1));
        }
    }
}

pub(super) fn check_test_targets(
    path: &str,
    contents: &str,
    app_tests: &BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    let mut lines = contents.lines().enumerate().peekable();
    while let Some((index, line)) = lines.next() {
        let key = line.trim_start().trim_start_matches("- ");
        let Some(run) = key.strip_prefix("run:").map(str::trim) else {
            continue;
        };
        if !run.starts_with(['|', '>']) {
            check_command(
                path,
                index + 1,
                run.trim_matches(['\'', '"']),
                app_tests,
                errors,
            );
            continue;
        }

        let indent = line.len() - key.len();
        let mut command = String::new();
        let mut command_line = index + 1;
        while let Some((index, line)) = lines.next_if(|(_, line)| {
            line.trim().is_empty() || line.len() - line.trim_start().len() > indent
        }) {
            let line = line.trim();
            if command.is_empty() {
                command_line = index + 1;
            }
            if let Some(continued) = line.strip_suffix(['\\', '`']) {
                command.push_str(continued);
                continue;
            }
            command.push_str(line);
            if run.starts_with('>') && !line.is_empty() {
                command.push(' ');
                continue;
            }
            check_command(path, command_line, &command, app_tests, errors);
            command.clear();
        }
        check_command(path, command_line, &command, app_tests, errors);
    }
}

fn check_command(
    path: &str,
    line: usize,
    command: &str,
    app_tests: &BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    static CARGO_TEST: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:^|[;&|])[ \t]*(?:[A-Za-z_]\w*=\S+[ \t]+)*cargo[ \t]+(?:\+\S+[ \t]+)?test[ \t]+([^;&|]*)")
            .expect("cargo test command pattern")
    });
    let command = command.split('#').next().unwrap_or_default();
    for captures in CARGO_TEST.captures_iter(command) {
        let mut arguments = captures[1]
            .split_whitespace()
            .map(|argument| argument.trim_matches(['\'', '"']))
            .take_while(|argument| *argument != "--");
        let mut selects_app = false;
        let mut targets = Vec::new();
        while let Some(argument) = arguments.next() {
            let (option, value) = argument
                .split_once('=')
                .map_or((argument, None), |(option, value)| {
                    (option, Some(value.trim_matches(['\'', '"'])))
                });
            match option {
                "-p" | "--package" => {
                    selects_app |= value.or_else(|| arguments.next()) == Some("emp-app");
                }
                "-pemp-app" => selects_app = true,
                "--test" => targets.extend(value.or_else(|| arguments.next())),
                _ => {}
            }
        }
        if selects_app {
            for target in targets
                .into_iter()
                .filter(|target| !app_tests.contains(*target))
            {
                errors.push(format!(
                    "[workflow-test-targets] {path}:{line}: emp-app 没有名为 {target} 的集成测试目标"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_python_on_main_text_rules() {
        for (line, valid) in [
            ("run: cargo xtask check", true),
            ("run: echo pythonic pipeline build.pyc", true),
            ("run: python3 script.py", false),
            ("run: pip install package", false),
            ("run: ./script.py", false),
            ("/usr/bin/python3.12", false),
            ("cargo fmt && pip3 install package", false),
            ("uses: actions/setup-python@v5", false),
            ("on: workflow_dispatch\nrun: python build.py", false),
        ] {
            let mut errors = Vec::new();
            check(".github/workflows/check.yml", line, &mut errors);
            assert_eq!(errors.is_empty(), valid, "{line}: {errors:?}");
            if !valid {
                assert!(errors[0].contains("[no-python-on-main] .github/workflows/check.yml:"));
                assert!(errors[0].contains("把它加入清单并说明"));
            }
        }
    }
}
