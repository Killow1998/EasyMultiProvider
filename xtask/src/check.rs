mod arch;
mod comments;
mod modules;
mod prose;
mod workflows;

use crate::{Result, project_root};
use std::fs;
use std::path::Path;
use std::process::Command;

pub fn run(args: &[String]) -> Result {
    if !args.is_empty() {
        return Err("usage: cargo xtask check".into());
    }
    let root = project_root();
    let mut errors = Vec::new();
    arch::check(&root, &mut errors)?;
    modules::check(&root, &mut errors)?;
    let list_files = [
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    ];
    let paths = git_paths(&root, &list_files)?;
    comments::check(&root, &paths, &mut errors)?;
    for path in paths {
        if !root.join(&path).is_file() {
            continue;
        }
        if path.starts_with("crates/") && path.split('/').nth(2) == Some("tests") {
            test_boundary(
                &path,
                &fs::read(root.join(&path)).map_err(|e| format!("{path}: {e}"))?,
                &mut errors,
            );
        } else if prose::includes(&path) {
            prose::check(&path, &read(&root, &path)?, &mut errors);
        } else if path.starts_with(".github/workflows/") && path.ends_with(".yml") {
            workflows::check(&path, &read(&root, &path)?, &mut errors);
        }
    }
    if errors.is_empty() {
        println!(
            "check: arch, module-version, test-boundary, docs-prose, no-python-on-main, comments, disguised-sleep, msvc-only passed"
        );
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn read(root: &Path, path: &str) -> Result<String> {
    fs::read_to_string(root.join(path)).map_err(|error| format!("{path}: {error}"))
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|error| format!("git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("git output must be UTF-8: {error}"))
}

fn git_paths(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let mut paths = git(root, args)?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn test_boundary(path: &str, bytes: &[u8], errors: &mut Vec<String>) {
    for (index, line) in String::from_utf8_lossy(bytes).lines().enumerate() {
        if let Some(forbidden) = ["ServerState", "emp_app::"]
            .into_iter()
            .find(|word| line.contains(*word))
        {
            errors.push(format!("[test-boundary] {path}:{}: {forbidden} 跨越黑盒边界；通过 EMP 二进制和 HTTP 观察行为，预期版本使用 env!(\"CARGO_PKG_VERSION\")", index + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boundary_checks_binary_and_http_access() {
        let cases: &[(&[u8], usize)] = &[
            (b"Command::new(binary); http.get(\"/api/config\");", 0),
            (b"let state: ServerState;\nemp_app::VERSION;", 2),
        ];
        for &(contents, count) in cases {
            let mut errors = Vec::new();
            test_boundary("crates/emp-app/tests/e2e.rs", contents, &mut errors);
            assert_eq!(errors.len(), count);
            if count > 0 {
                assert!(errors[0].contains("e2e.rs:1: ServerState"));
                assert!(errors[1].contains("e2e.rs:2: emp_app::"));
            }
        }
    }
}
