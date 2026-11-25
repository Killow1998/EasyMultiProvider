//! These text rules catch accidental CI changes. YAML and Actions semantics belong to actionlint.
//! Shell syntax, heredocs and deliberate indirection are outside this check's scope.

const ALLOWED: &[(&str, &str)] = &[(
    ".github/workflows/module-e2e.yml",
    "M04 验收真实 Aero 包；Python 仅用于安装 PyInstaller 和打包，场景由 Rust 执行",
)];

pub(super) fn strip_comment(line: &str) -> &str {
    line.match_indices('#')
        .find(|(index, _)| {
            *index == 0
                || line[..*index].ends_with(|character: char| {
                    character.is_whitespace() || ";&|()<>".contains(character)
                })
        })
        .map_or(line, |(index, _)| &line[..index])
}

fn check_python(path: &str, contents: &str, errors: &mut Vec<String>) {
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

pub(super) fn check(path: &str, contents: &str, errors: &mut Vec<String>) {
    check_python(path, contents, errors);
    for (index, line) in contents.lines().enumerate() {
        let code = strip_comment(line);
        let mut words = code.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        if words.any(|word| word == "cargo") && words.any(|word| matches!(word, "test" | "nextest"))
        {
            errors.push(format!(
                "[workflow-cargo-test] {path}:{}: 把测试调用加到 xtask/src/ci.rs 的 profile，工作流改用 cargo xtask ci <profile>",
                index + 1
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn direct_cargo_tests_are_rejected() {
        for command in [
            "run: cargo test",
            "run: cargo +1.93.1 test --locked",
            "run: cargo nextest run",
            "if ! cargo --color always --locked test; then",
            "run: cargo --config 'net.retry = 3' test --locked",
        ] {
            let mut errors = Vec::new();
            check("workflow.yml", &format!("steps:\n{command}"), &mut errors);
            assert_eq!(errors.len(), 1, "{command}: {errors:?}");
            assert!(errors[0].starts_with("[workflow-cargo-test] workflow.yml:2:"));
        }
    }

    #[test]
    fn other_cargo_commands_and_comments_are_allowed() {
        let mut errors = Vec::new();
        check(
            "workflow.yml",
            "run: cargo xtask ci tray-actions\nrun: cargo build --bins\nrun: cargo clippy # cargo test",
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn existing_workflows_pass() {
        let root = crate::project_root();
        let workflows = root.join(".github/workflows");
        let mut errors = Vec::new();
        for entry in fs::read_dir(workflows).unwrap() {
            let file = entry.unwrap().path();
            if matches!(
                file.extension().and_then(|ext| ext.to_str()),
                Some("yml" | "yaml")
            ) {
                let path = file
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let contents = fs::read_to_string(file).unwrap();
                check(&path, &contents, &mut errors);
            }
        }
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }

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
            check_python(".github/workflows/check.yml", line, &mut errors);
            assert_eq!(errors.is_empty(), valid, "{line}: {errors:?}");
            if !valid {
                assert!(errors[0].contains("[no-python-on-main] .github/workflows/check.yml:"));
                assert!(errors[0].contains("把它加入清单并说明"));
            }
        }
    }
}
