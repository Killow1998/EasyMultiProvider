mod cargo_tests;
mod yaml;

use regex::Regex;
use std::{collections::BTreeSet, sync::LazyLock};
use yaml::{Node, NodeExt as _};

const CONTEXTS: &[&str] = &[
    "github", "env", "vars", "secrets", "inputs", "job", "jobs", "steps", "runner", "strategy",
    "matrix", "needs",
];
const WORKFLOW_CONTEXTS: &[(&str, &[&str])] = &[
    ("env", &["github", "secrets", "inputs", "vars"]),
    ("concurrency", &["github", "inputs", "vars"]),
];
const JOB_CONTEXTS: &[(&str, &[&str])] = &[
    (
        "env",
        &[
            "github", "needs", "strategy", "matrix", "vars", "secrets", "inputs",
        ],
    ),
    (
        "runs-on",
        &["github", "needs", "strategy", "matrix", "vars", "inputs"],
    ),
    ("strategy", &["github", "needs", "vars", "inputs"]),
    (
        "concurrency",
        &["github", "needs", "strategy", "matrix", "inputs", "vars"],
    ),
    ("if", &["github", "needs", "vars", "inputs"]),
    (
        "timeout-minutes",
        &["github", "needs", "strategy", "matrix", "vars", "inputs"],
    ),
];

macro_rules! expression_pattern {
    () => {
        r"\$\{\{((?:'(?:[^']|'')*'|[^'}])*)\}\}"
    };
}
use expression_pattern;

static EXPRESSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(expression_pattern!()).expect("Actions expression pattern"));

const ALLOWED: &[(&str, &str)] = &[(
    ".github/workflows/module-e2e.yml",
    "M04 验收真实 Aero 包；Python 仅用于安装 PyInstaller 和打包，场景由 Rust 执行",
)];

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

pub(super) fn check(
    path: &str,
    contents: &str,
    app_tests: &BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    check_python(path, contents, errors);
    let workflow = match yaml::parse(contents) {
        Ok(Some(workflow)) => workflow,
        Ok(None) => return,
        Err(error) => {
            errors.push(format!("[workflow-yaml] {path}: {error}"));
            return;
        }
    };
    let mut check = WorkflowCheck { path, errors };

    for &(field, allowed) in WORKFLOW_CONTEXTS {
        if let Some(value) = workflow.get(field) {
            check.contexts(field, value, allowed, false);
        }
    }
    if let Some(jobs) = workflow.get("jobs").and_then(Node::entries) {
        for (id, job) in jobs {
            let job_path = format!("jobs.{id}");
            for &(field, allowed) in JOB_CONTEXTS {
                if let Some(value) = job.get(field) {
                    check.contexts(
                        &format!("{job_path}.{field}"),
                        value,
                        allowed,
                        field == "if",
                    );
                }
            }
            let matrix = job
                .get("strategy")
                .and_then(|strategy| strategy.get("matrix"));
            let default_directory = working_directory(job)
                .or_else(|| working_directory(&workflow))
                .unwrap_or_default();
            let steps = job.get("steps").and_then(Node::as_sequence);
            for step in steps.into_iter().flatten() {
                if let Some(run_node) = step.get("run")
                    && let Some(run) = run_node.as_str()
                {
                    let directory = step
                        .get("working-directory")
                        .and_then(Node::as_str)
                        .unwrap_or(default_directory);
                    for target in cargo_tests::missing(run, directory, matrix, app_tests) {
                        let line = run_node.line();
                        check.errors.push(format!(
                            "[workflow-test-targets] {path}:{line}: emp-app 没有名为 {target} 的集成测试目标"
                        ));
                    }
                }
            }
        }
    }
}

fn working_directory(node: &Node) -> Option<&str> {
    node.get("defaults")?
        .get("run")?
        .get("working-directory")?
        .as_str()
}

struct WorkflowCheck<'a> {
    path: &'a str,
    errors: &'a mut Vec<String>,
}

impl WorkflowCheck<'_> {
    fn contexts(&mut self, key: &str, node: &Node, allowed: &[&str], implicit: bool) {
        if let Some(map) = node.entries() {
            for (name, value) in map {
                self.contexts(&format!("{key}.{name}"), value, allowed, false);
            }
        } else if let Some(sequence) = node.as_sequence() {
            for (index, value) in sequence.iter().enumerate() {
                self.contexts(&format!("{key}[{index}]"), value, allowed, false);
            }
        } else if let Some(text) = node.as_str() {
            let expressions = EXPRESSION
                .captures_iter(text)
                .map(|captures| captures.extract::<1>().1[0]);
            if implicit && !text.contains("${{") {
                self.expression_contexts(key, node.line(), text, allowed);
            } else {
                for expression in expressions {
                    self.expression_contexts(key, node.line(), expression, allowed);
                }
            }
        }
    }

    fn expression_contexts(&mut self, key: &str, line: usize, expression: &str, allowed: &[&str]) {
        static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"'(?:[^']|'')*'|[a-zA-Z_][a-zA-Z0-9_-]*|[^\s]")
                .expect("Actions expression token pattern")
        });
        let mut tokens = TOKEN
            .find_iter(expression)
            .map(|token| token.as_str())
            .peekable();
        let mut previous = "";
        while let Some(token) = tokens.next() {
            if previous != "."
                && tokens.peek() != Some(&"(")
                && let Some(context) = CONTEXTS
                    .iter()
                    .find(|context| token.eq_ignore_ascii_case(context))
                && !allowed.contains(context)
            {
                let path = self.path;
                self.errors.push(format!(
                    "[workflow-contexts] {path}:{line}: {key} 不能使用 {context} 上下文"
                ));
            }
            previous = token;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workflow_errors(contents: &str) -> Vec<String> {
        let app_tests = ["exit_process", "http_support"].map(str::to_owned).into();
        let mut errors = Vec::new();
        check("workflow.yml", contents, &app_tests, &mut errors);
        errors
    }

    #[test]
    fn runner_temp_is_available_in_step_env_only() {
        let errors = workflow_errors(
            r#"env:
  CODEX_HOME: ${{ runner.temp }}/home
jobs:
  diagnose:
    env:
      CODEX_HOME: ${{ runner.temp }}/home
    steps:
      - env:
          CODEX_HOME: ${{ runner.temp }}/home
        run: echo "$CODEX_HOME"
"#,
        );
        assert_eq!(
            errors,
            [
                "[workflow-contexts] workflow.yml:2: env.CODEX_HOME 不能使用 runner 上下文",
                "[workflow-contexts] workflow.yml:6: jobs.diagnose.env.CODEX_HOME 不能使用 runner 上下文",
            ]
        );
    }

    #[test]
    fn context_rules_follow_workflow_and_job_fields() {
        let errors = workflow_errors(
            r#"env:
  A: ${{ toJSON(steps) }}
  B: ${{ job.status }}
  C: ${{ env.VALUE }}
  D: ${{ github.event.runner || 'steps '' }} env' }}
concurrency: ${{ matrix.runner }}
jobs:
  diagnose:
    env:
      A: ${{ steps.main.outcome }}
      B: ${{ Job.status }}
      C: ${{ env.VALUE }}
      D: ${{ matrix.runner }}
    runs-on: ["${{ steps.main.outputs.runner }}"]
    strategy:
      matrix: ${{ fromJSON(needs.prepare.outputs.matrix || steps.prepare.outputs.matrix) }}
    concurrency:
      group: ${{ matrix.runner }}
      cancel-in-progress: ${{ job.status == 'success' }}
    if: matrix.runner == 'linux'
    timeout-minutes: ${{ secrets.TIMEOUT }}
"#,
        );
        assert_eq!(errors.len(), 12, "{errors:#?}");
        for (field, context) in [
            ("env.A", "steps"),
            ("env.B", "job"),
            ("env.C", "env"),
            ("concurrency", "matrix"),
            ("jobs.diagnose.env.A", "steps"),
            ("jobs.diagnose.env.B", "job"),
            ("jobs.diagnose.env.C", "env"),
            ("jobs.diagnose.runs-on[0]", "steps"),
            ("jobs.diagnose.strategy.matrix", "steps"),
            ("jobs.diagnose.concurrency.cancel-in-progress", "job"),
            ("jobs.diagnose.if", "matrix"),
            ("jobs.diagnose.timeout-minutes", "secrets"),
        ] {
            assert!(
                errors
                    .iter()
                    .any(|error| error.ends_with(&format!("{field} 不能使用 {context} 上下文"))),
                "{errors:#?}"
            );
        }
    }

    #[test]
    fn test_targets_are_checked_after_yaml_and_shell_continuations() {
        let errors = workflow_errors(
            r#"jobs:
  native:
    steps:
      - run: cargo test -p emp-app
          --test retired_plain
      - run: >-
          cargo +1.93.1 test --package emp-app
          --test retired_folded
      - run: |
          cargo test -pemp-app \
            --test retired_literal
          cargo test -pemp-app --test exit_process -- --test forwarded_filter
          # cargo test -pemp-app --test comment
          echo "cargo test -pemp-app --test printed"
"#,
        );
        assert_eq!(errors.len(), 3, "{errors:#?}");
        for target in ["retired_plain", "retired_folded", "retired_literal"] {
            assert!(
                errors
                    .iter()
                    .any(|error| error.ends_with(&format!("{target} 的集成测试目标"))),
                "{errors:#?}"
            );
        }
    }

    #[test]
    fn test_targets_are_checked_when_manifest_selects_emp_app() {
        let errors = workflow_errors(
            r#"jobs:
  native:
    steps:
      - run: cargo test --manifest-path crates/emp-app/Cargo.toml --test retired_at_root
      - run: cargo test --manifest-path=".\crates\emp-app\Cargo.toml" --test retired_windows
      - working-directory: crates/emp-codex
        run: cargo test --manifest-path ../emp-app/Cargo.toml --test retired_relative
      - run: cargo test --manifest-path crates/emp-codex/Cargo.toml --test other_package
"#,
        );
        assert_eq!(errors.len(), 3, "{errors:#?}");
        for target in ["retired_at_root", "retired_windows", "retired_relative"] {
            assert!(
                errors
                    .iter()
                    .any(|error| error.ends_with(&format!("{target} 的集成测试目标"))),
                "{errors:#?}"
            );
        }
    }

    #[test]
    fn test_target_expressions_expand_static_matrix_values() {
        let workflow = r#"jobs:
  native:
    strategy:
      matrix:
        target: [exit_process]
        include:
          - target: http_support
    steps:
      - run: cargo test -pemp-app --test "${{ matrix.target }}"
      - run: cargo test -pemp-app --test=${{ matrix.target }}
      - run: cargo test -pemp-app --test "${{ inputs.target }}"
"#;
        assert!(workflow_errors(workflow).is_empty());
        let errors = workflow_errors(
            &workflow
                .replace("[exit_process]", "[exit_process, retired_axis]")
                .replace("target: http_support", "target: retired_include"),
        );
        assert_eq!(errors.len(), 4, "{errors:#?}");
        for target in ["retired_axis", "retired_include"] {
            assert_eq!(
                errors
                    .iter()
                    .filter(|error| error.ends_with(&format!("{target} 的集成测试目标")))
                    .count(),
                2,
                "{errors:#?}"
            );
        }
    }

    #[test]
    fn existing_workflows_pass() {
        let root = crate::project_root();
        let workflows = root.join(".github/workflows");
        let mut errors = Vec::new();
        let (_, app_tests) = super::super::arch::check(&root, &mut errors).unwrap();
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
                check(&path, &contents, &app_tests, &mut errors);
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
