mod cargo_tests;
mod yaml;

use regex::Regex;
use saphyr::SafelyIndex as _;
use std::{collections::BTreeSet, sync::LazyLock};
use yaml::Node;

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
    check.mapping_keys(&workflow.root);

    for &(field, allowed) in WORKFLOW_CONTEXTS {
        if let Some(value) = workflow.root.get(field) {
            check.contexts(field, value, allowed, false);
        }
    }
    if let Some(jobs) = workflow
        .root
        .get("jobs")
        .and_then(|node| node.data.as_mapping())
    {
        for (id, job) in jobs {
            let id = id.data.as_str().unwrap_or("<非字符串键>");
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
            if let Some(exclude) = job
                .get("strategy")
                .and_then(|strategy| strategy.get("matrix"))
                .and_then(|matrix| matrix.data.as_mapping())
                .and_then(|matrix| {
                    matrix
                        .keys()
                        .find(|key| key.data.as_str() == Some("exclude"))
                })
            {
                let line = exclude.span.start.line();
                check.errors.push(format!(
                    "[workflow-test-targets] {path}:{line}: {job_path}.strategy.matrix 不能使用 exclude；请用 include 列出要运行的配置，并把测试目标静态写进命令"
                ));
            }
            let steps = job.get("steps").and_then(|node| node.data.as_sequence());
            for step in steps.into_iter().flatten() {
                if let Some(run_node) = step.get("run")
                    && let Some(run) = run_node.data.as_str()
                {
                    cargo_tests::check(
                        path,
                        run,
                        workflow.script_lines(run_node),
                        app_tests,
                        check.errors,
                    );
                }
            }
        }
    }
}

struct WorkflowCheck<'a> {
    path: &'a str,
    errors: &'a mut Vec<String>,
}

impl WorkflowCheck<'_> {
    fn mapping_keys(&mut self, node: &Node) {
        if let Some(map) = node.data.as_mapping() {
            for (key, value) in map {
                if key.data.as_str().is_some_and(|name| name.contains("${{")) {
                    let path = self.path;
                    let line = key.span.start.line();
                    self.errors.push(format!(
                        "[workflow-yaml] {path}:{line}: 映射键不能使用表达式；请使用固定键名，把表达式放进支持表达式的值"
                    ));
                }
                self.mapping_keys(value);
            }
        } else if let Some(sequence) = node.data.as_sequence() {
            for value in sequence {
                self.mapping_keys(value);
            }
        }
    }

    fn contexts(&mut self, key: &str, node: &Node, allowed: &[&str], implicit: bool) {
        if let Some(map) = node.data.as_mapping() {
            for (name, value) in map {
                let name = name.data.as_str().unwrap_or("<非字符串键>");
                self.contexts(&format!("{key}.{name}"), value, allowed, false);
            }
        } else if let Some(sequence) = node.data.as_sequence() {
            for (index, value) in sequence.iter().enumerate() {
                self.contexts(&format!("{key}[{index}]"), value, allowed, false);
            }
        } else if let Some(text) = node.data.as_str() {
            let line = node.span.start.line();
            let expressions = EXPRESSION
                .captures_iter(text)
                .map(|captures| captures.extract::<1>().1[0]);
            if implicit && !text.contains("${{") {
                self.expression_contexts(key, line, text, allowed);
            } else {
                for expression in expressions {
                    self.expression_contexts(key, line, expression, allowed);
                }
            }
        }
    }

    fn expression_contexts(&mut self, key: &str, line: usize, expression: &str, allowed: &[&str]) {
        static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(concat!(
                r"'(?:[^']|'')*'|-?(?:0[xX][0-9a-fA-F]+|[0-9]+(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?)|",
                r"([a-zA-Z_][a-zA-Z0-9_-]*)|[^\s]",
            ))
            .expect("Actions expression token pattern")
        });
        let mut tokens = TOKEN.captures_iter(expression).peekable();
        let mut previous = "";
        while let Some(token) = tokens.next() {
            if previous != "."
                && let Some(identifier) = token.get(1)
                && tokens.peek().is_none_or(|next| &next[0] != "(")
            {
                let context = identifier.as_str().to_ascii_lowercase();
                if !matches!(context.as_str(), "true" | "false" | "null")
                    && !allowed.contains(&context.as_str())
                {
                    let path = self.path;
                    self.errors.push(format!(
                        "[workflow-contexts] {path}:{line}: {key} 不能使用 {context} 上下文"
                    ));
                }
            }
            previous = token.get_match().as_str();
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
    fn rejects_unknown_expression_roots() {
        let errors = workflow_errors(
            r#"jobs:
  native:
    env:
      CODEX_HOME: ${{ runer.temp }}/home
    if: contains(github.event.labels, 'runner '' test') || github.event.*.label == null || true || false || 0xff == -2.99e-2
"#,
        );
        assert_eq!(
            errors,
            [
                "[workflow-contexts] workflow.yml:4: jobs.native.env.CODEX_HOME 不能使用 runer 上下文"
            ]
        );
    }

    #[test]
    fn rejects_expressions_in_mapping_keys() {
        let errors = workflow_errors(
            r#"jobs:
  native:
    env: { "${{ runner.os }}": "1" }
    steps:
      - with:
          "prefix-${{ github.ref }}": value
        run: echo ok
"#,
        );
        assert_eq!(errors.len(), 2, "{errors:#?}");
        for (error, line) in errors.iter().zip([3, 6]) {
            assert!(error.starts_with(&format!("[workflow-yaml] workflow.yml:{line}:")));
            assert!(error.contains("固定键名"));
        }
    }

    #[test]
    fn rejects_duplicate_run_keys_at_the_duplicate_line() {
        let errors = workflow_errors(
            r#"jobs:
  native:
    steps:
      - run: cargo test -p emp-app --test retired
        "run": cargo test -p emp-app --test exit_process
"#,
        );
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert!(errors[0].starts_with("[workflow-yaml] workflow.yml:"));
        assert!(errors[0].contains("duplicated key in mapping"));
        assert!(errors[0].contains("line 5 column"));
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
          echo "preparing"

          cargo test -pemp-app \
            --test retired_literal
          cargo test -pemp-app --test retired_after_continuation
          cargo test -pemp-app --test exit_process -- --test forwarded_filter
          cargo test -p emp-transport --test http_client
          cargo test --test unselected_package
          # cargo test -pemp-app --test comment
          echo "cargo test -pemp-app --test printed"
"#,
        );
        assert_eq!(errors.len(), 4, "{errors:#?}");
        for (error, (line, target)) in errors.iter().zip([
            (4, "retired_plain"),
            (6, "retired_folded"),
            (12, "retired_literal"),
            (14, "retired_after_continuation"),
        ]) {
            assert_eq!(
                error,
                &format!(
                    "[workflow-test-targets] workflow.yml:{line}: emp-app 没有名为 {target} 的集成测试目标"
                ),
            );
        }
    }

    #[test]
    fn rejects_manifest_paths_in_cargo_commands() {
        let errors = workflow_errors(
            r#"jobs:
  native:
    steps:
      - run: cargo test --manifest-path crates/emp-app/Cargo.toml -p emp-transport --test http_client
      - run: cargo +1.93.1 build --manifest-path=".\crates\emp-app\Cargo.toml"
      - run: cargo test -p emp-app --test exit_process -- --manifest-path forwarded_filter
"#,
        );
        assert_eq!(errors.len(), 2, "{errors:#?}");
        for (error, line) in errors.iter().zip([4, 5]) {
            assert!(error.starts_with(&format!("[workflow-test-targets] workflow.yml:{line}:")));
            assert!(error.contains("--manifest-path"));
            assert!(error.contains("-p <crate>"));
        }
    }

    #[test]
    fn rejects_cd_before_cargo_in_a_run() {
        let errors = workflow_errors(
            r#"jobs:
  native:
    steps:
      - run: cd crates/emp-app && cargo test -p emp-app --test exit_process
      - run: |
          cd crates/emp-app
          cargo build -p emp-app
      - working-directory: crates/emp-app
        run: cargo test -p emp-app --test exit_process
"#,
        );
        assert_eq!(errors.len(), 2, "{errors:#?}");
        for (error, line) in errors.iter().zip([4, 7]) {
            assert!(error.starts_with(&format!("[workflow-test-targets] workflow.yml:{line}:")));
            assert!(error.contains("cd"));
            assert!(error.contains("working-directory:"));
            assert!(error.contains("-p"));
        }
    }

    #[test]
    fn rejects_matrix_exclude_and_dynamic_test_targets() {
        let errors = workflow_errors(
            r#"jobs:
  native:
    strategy:
      matrix:
        target: [exit_process, retired]
        include:
          - target: http_support
        exclude:
          - target: retired
    steps:
      - run: cargo test -pemp-app --test "${{ matrix.target }}"
      - run: cargo test -p emp-transport --test=${{ inputs.target }}
      - run: cargo test -pemp-app --test exit_process -- --test "${{ inputs.filter }}"
"#,
        );
        assert_eq!(errors.len(), 3, "{errors:#?}");
        for (error, (line, option)) in
            errors
                .iter()
                .zip([(8, "exclude"), (11, "--test"), (12, "--test")])
        {
            assert!(error.starts_with(&format!("[workflow-test-targets] workflow.yml:{line}:")));
            assert!(error.contains(option));
            assert!(error.contains("把测试目标静态写进命令"));
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
