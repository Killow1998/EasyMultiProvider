const ALLOWED: &[(&str, &str)] = &[];

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
