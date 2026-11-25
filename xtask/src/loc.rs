use crate::Result;
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Cursor, Read, Write},
    process::{Command, Stdio},
};

const USAGE: &str =
    "usage: cargo xtask loc <base> <head> [--subtract-merge <commit>]... [--prefix <path>]...";

type Files<T> = BTreeMap<String, T>;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Lines {
    production: i64,
    tests: i64,
    examples: i64,
}

impl Lines {
    fn add(self, other: Self) -> Self {
        Self {
            production: self.production + other.production,
            tests: self.tests + other.tests,
            examples: self.examples + other.examples,
        }
    }

    fn delta(self, base: Self) -> Self {
        Self {
            production: self.production - base.production,
            tests: self.tests - base.tests,
            examples: self.examples - base.examples,
        }
    }

    fn totals(files: &Files<Self>, prefix: &str) -> Self {
        files
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .fold(Self::default(), |total, (_, lines)| total.add(*lines))
    }
}

struct Options<'a> {
    base: &'a str,
    head: &'a str,
    merges: Vec<&'a str>,
    prefixes: Vec<&'a str>,
}

impl<'a> Options<'a> {
    fn parse(args: &'a [String]) -> Result<Self> {
        let [base, head, rest @ ..] = args else {
            return Err(USAGE.into());
        };
        if [base, head]
            .iter()
            .any(|rev| rev.is_empty() || rev.starts_with('-'))
        {
            return Err(USAGE.into());
        }
        let mut options = Self {
            base,
            head,
            merges: Vec::new(),
            prefixes: Vec::new(),
        };
        for pair in rest.chunks(2) {
            match pair {
                [flag, value]
                    if flag == "--subtract-merge"
                        && !value.is_empty()
                        && !value.starts_with('-') =>
                {
                    options.merges.push(value);
                }
                [flag, value] if flag == "--prefix" => options.prefixes.push(value),
                _ => return Err(USAGE.into()),
            }
        }
        Ok(options)
    }
}

pub(super) fn run(args: &[String]) -> Result {
    let options = Options::parse(args)?;
    let counter = Counter::new();
    let base = counter.count(&read_tree(options.base)?);
    let head = counter.count(&read_tree(options.head)?);
    let before = Lines::totals(&base, "");
    let after = Lines::totals(&head, "");
    let interval = after.delta(before);
    println!("production\ttests\texamples\trevision or change");
    print_total(options.base, before);
    print_total(options.head, after);
    print_delta("interval", interval);

    let mut phase = interval;
    for merge in options.merges {
        let parent = format!("{merge}^1");
        let before = Lines::totals(&counter.count(&read_tree(&parent)?), "");
        let after = Lines::totals(&counter.count(&read_tree(merge)?), "");
        let upstream = after.delta(before);
        print_total(&parent, before);
        print_total(merge, after);
        print_delta(&format!("upstream {merge}"), upstream);
        phase = phase.delta(upstream);
    }
    print_delta("phase", phase);

    println!(
        "\nproduction base\thead\tdelta\ttests base\thead\tdelta\texamples base\thead\tdelta\tpath"
    );
    for prefix in options.prefixes {
        print_path(
            prefix,
            Lines::totals(&base, prefix),
            Lines::totals(&head, prefix),
        );
    }
    let paths: BTreeSet<_> = base.keys().chain(head.keys()).collect();
    let mut changed: Vec<_> = paths
        .into_iter()
        .filter_map(|path| {
            let before = base.get(path).copied().unwrap_or_default();
            let after = head.get(path).copied().unwrap_or_default();
            (before != after).then_some((path, before, after))
        })
        .collect();
    changed.sort_by_key(|(path, before, after)| (before.production - after.production, *path));
    for (path, before, after) in changed {
        print_path(path, before, after);
    }
    Ok(())
}

fn print_total(label: &str, lines: Lines) {
    println!(
        "{}\t{}\t{}\t{label}",
        lines.production, lines.tests, lines.examples
    );
}

fn print_delta(label: &str, lines: Lines) {
    println!(
        "{:+}\t{:+}\t{:+}\t{label}",
        lines.production, lines.tests, lines.examples
    );
}

fn print_path(path: &str, base: Lines, head: Lines) {
    let delta = head.delta(base);
    println!(
        "{}\t{}\t{:+}\t{}\t{}\t{:+}\t{}\t{}\t{:+}\t{path}",
        base.production,
        head.production,
        delta.production,
        base.tests,
        head.tests,
        delta.tests,
        base.examples,
        head.examples,
        delta.examples,
    );
}

fn git(command: &mut Command) -> Result<Vec<u8>> {
    let output = command
        .output()
        .map_err(|error| format!("{command:?}: {error}"))?;
    stdout(command, output)
}

fn stdout(command: &Command, output: std::process::Output) -> Result<Vec<u8>> {
    if !output.status.success() {
        return Err(format!(
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn git_with_input(command: &mut Command, input: Vec<u8>) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{command:?}: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("git stdin unavailable")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child
        .wait_with_output()
        .map_err(|error| format!("{command:?}: {error}"))?;
    writer
        .join()
        .map_err(|_| "git stdin writer panicked")?
        .map_err(|error| format!("{command:?} stdin: {error}"))?;
    stdout(command, output)
}

fn read_tree(rev: &str) -> Result<Files<String>> {
    let output = git(Command::new("git").args(["ls-tree", "-rz", "--full-tree", rev, "--"]))?;
    let tree = String::from_utf8(output).map_err(|error| format!("git ls-tree {rev}: {error}"))?;
    let mut blobs = Vec::new();
    for entry in tree.split_terminator('\0') {
        let (metadata, path) = entry.split_once('\t').ok_or("invalid git ls-tree record")?;
        let [mode, "blob", oid] = metadata.split(' ').collect::<Vec<_>>()[..] else {
            continue;
        };
        if mode != "120000" && path.ends_with(".rs") {
            blobs.push((path.to_owned(), oid));
        }
    }
    if blobs.is_empty() {
        return Ok(Files::new());
    }

    let requests = blobs
        .iter()
        .map(|(_, oid)| format!("{oid}\n"))
        .collect::<String>();
    let output = git_with_input(
        Command::new("git").args(["cat-file", "--batch"]),
        requests.into_bytes(),
    )?;
    let mut batch = Cursor::new(output);
    let mut files = Files::new();
    for (path, oid) in blobs {
        let mut header = String::new();
        batch
            .read_line(&mut header)
            .map_err(|error| format!("{rev}:{path}: {error}"))?;
        let size = match header.split_whitespace().collect::<Vec<_>>()[..] {
            [found, "blob", size] if found == oid => size.parse::<usize>(),
            _ => {
                return Err(format!(
                    "{rev}:{path}: invalid git cat-file record: {}",
                    header.trim()
                ));
            }
        }
        .map_err(|error| format!("{rev}:{path}: {error}"))?;
        let mut bytes = vec![0; size];
        batch
            .read_exact(&mut bytes)
            .map_err(|error| format!("{rev}:{path}: {error}"))?;
        let mut delimiter = [0];
        batch
            .read_exact(&mut delimiter)
            .map_err(|error| format!("{rev}:{path}: {error}"))?;
        if delimiter != [b'\n'] {
            return Err(format!("{rev}:{path}: missing Git blob delimiter"));
        }
        let source = String::from_utf8(bytes).map_err(|error| format!("{rev}:{path}: {error}"))?;
        files.insert(path, source);
    }
    Ok(files)
}

struct Counter {
    test_module: Regex,
    path_attribute: Regex,
    inline_test: Regex,
}

impl Counter {
    fn new() -> Self {
        Self {
            test_module: Regex::new(
                r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;",
            )
            .expect("test module pattern"),
            path_attribute: Regex::new(r#"#\[path\s*=\s*"([^"]+)"\]"#)
                .expect("path attribute pattern"),
            inline_test: Regex::new(r"#\[cfg\((?:test|all\(test[^\]]*)\)\]")
                .expect("inline test pattern"),
        }
    }

    fn count(&self, files: &Files<String>) -> Files<Lines> {
        let (test_files, test_dirs) = self.test_paths(files);
        files
            .iter()
            .filter_map(|(path, source)| {
                if path.starts_with("xtask/") || path.starts_with("crates/emp-test-support/") {
                    return None;
                }
                let total = source.lines().count() as i64;
                let mut lines = Lines::default();
                if test_files.contains(path)
                    || path
                        .match_indices('/')
                        .any(|(index, _)| test_dirs.contains(&path[..index]))
                {
                    lines.tests = total;
                } else {
                    lines.tests = self.inline_test_lines(source);
                    let remaining = total - lines.tests;
                    if path.rsplit_once('/').is_some_and(|(directory, _)| {
                        directory.split('/').any(|part| part == "examples")
                    }) {
                        lines.examples = remaining;
                    } else {
                        lines.production = remaining;
                    }
                }
                Some((path.clone(), lines))
            })
            .collect()
    }

    fn test_paths(&self, files: &Files<String>) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut roots = BTreeSet::new();
        let mut subtrees = BTreeSet::new();
        for (path, source) in files {
            let (directory, filename) = path.rsplit_once('/').unwrap_or(("", path));
            let module_dir = if matches!(filename, "mod.rs" | "lib.rs" | "main.rs") {
                directory
            } else {
                path.trim_end_matches(".rs")
            };
            for declaration in self.test_module.captures_iter(source) {
                let module = join_path(module_dir, &declaration[1]);
                if let Some(override_path) = self.path_attribute.captures(&declaration[0]) {
                    roots.insert(join_path(directory, &override_path[1]));
                } else {
                    roots.insert(format!("{module}.rs"));
                    roots.insert(format!("{module}/mod.rs"));
                }
                subtrees.insert(module);
            }
            if filename == "tests.rs"
                || filename.ends_with("_tests.rs")
                || path
                    .split('/')
                    .any(|part| matches!(part, "tests" | "benches"))
                || source.trim_start().starts_with("#![cfg(test)]")
            {
                roots.insert(path.clone());
            }
        }
        for path in roots.iter().filter(|path| files.contains_key(*path)) {
            let module_dir = path
                .strip_suffix("/mod.rs")
                .unwrap_or_else(|| path.trim_end_matches(".rs"));
            subtrees.insert(module_dir.to_owned());
        }
        (roots, subtrees)
    }

    fn inline_test_lines(&self, source: &str) -> i64 {
        let bytes = source.as_bytes();
        let mut end = 0;
        let mut lines = 0;
        for attribute in self.inline_test.find_iter(source) {
            if attribute.start() < end {
                continue;
            }
            end = attribute.end();
            let mut depth = 0;
            while let Some(&byte) = bytes.get(end) {
                end += 1;
                match byte {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    b';' if depth == 0 => break,
                    b'#' if depth == 0 && bytes.get(end) == Some(&b'[') => {
                        end += bytes[end..]
                            .iter()
                            .position(|&byte| byte == b']')
                            .map_or(bytes.len() - end, |index| index + 1);
                    }
                    _ => {}
                }
            }
            lines += bytes[attribute.start()..end]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count() as i64
                + 1;
        }
        lines
    }
}

fn join_path(directory: &str, path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts = Vec::new();
    let base = if absolute { "" } else { directory };
    for part in base.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            _ => parts.push(part),
        }
    }
    let path = parts.join("/");
    if absolute { format!("/{path}") } else { path }
}

#[cfg(test)]
mod tests;
