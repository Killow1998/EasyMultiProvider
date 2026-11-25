use super::{expression_pattern, yaml::ScriptLines};
use regex::Regex;
use std::{collections::BTreeSet, sync::LazyLock};

pub(super) fn check(
    path: &str,
    script: &str,
    lines: ScriptLines,
    app_tests: &BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    static CONTINUATION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:\\|\x60)\r?\n").expect("shell continuation pattern"));
    static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r#"(?x)[\n;&|]+|\#[^\n]*|(?:"#,
            expression_pattern!(),
            r#"|"(?:\\.|[^"\\])*"|'[^']*'|[^\s;&|"'])+"#,
        ))
        .expect("shell command token pattern")
    });
    let mut continuations = CONTINUATION.find_iter(script).peekable();
    let joined = CONTINUATION.replace_all(script, "");
    let mut removed_bytes = 0;
    let mut previous = 0;
    let mut offset = 0;
    let mut command = Vec::new();
    let mut changed_directory = false;
    for (start, word) in TOKEN
        .find_iter(&joined)
        .map(|token| (token.start(), token.as_str()))
        .chain([(joined.len(), "\n")])
    {
        while let Some(continuation) =
            continuations.next_if(|span| span.start() <= start + removed_bytes)
        {
            offset += 1;
            removed_bytes += continuation.len();
        }
        offset += joined[previous..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        previous = start;

        match word.as_bytes()[0] {
            b'\n' | b';' | b'&' | b'|' => {
                check_command(
                    &command,
                    &mut changed_directory,
                    app_tests,
                    |offset, message| {
                        let line = lines.command(offset);
                        errors.push(format!("[workflow-test-targets] {path}:{line}: {message}"));
                    },
                );
                command.clear();
            }
            b'#' => {}
            _ => command.push((offset, unquote(word))),
        }
    }
}

fn unquote(word: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = word
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    word
}

fn check_command(
    command: &[(usize, &str)],
    changed_directory: &mut bool,
    app_tests: &BTreeSet<String>,
    mut report: impl FnMut(usize, &str),
) {
    let mut words = command.iter().copied().skip_while(|(_, word)| {
        word.split_once('=').is_some_and(|(name, _)| {
            name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    });
    let offset = match words.next() {
        Some((_, "cd")) => {
            *changed_directory = true;
            return;
        }
        Some((offset, "cargo")) => offset,
        _ => return,
    };
    if *changed_directory {
        report(
            offset,
            "同一 run 中不能先 cd 再调用 cargo；请使用 step 的 working-directory:，或在仓库根目录用 -p <crate>",
        );
    }

    let mut arguments = words
        .map(|(_, word)| word)
        .skip_while(|word| word.starts_with('+'))
        .take_while(|word| *word != "--")
        .peekable();
    let is_test = arguments.next_if_eq(&"test").is_some();
    let mut selects_app = false;
    let mut targets = BTreeSet::new();
    while let Some(argument) = arguments.next() {
        let (option, value) = argument
            .split_once('=')
            .map_or((argument, None), |(option, value)| {
                (option, Some(unquote(value)))
            });
        match option {
            "--manifest-path" => {
                report(
                    offset,
                    "工作流的 cargo 命令不能使用 --manifest-path；请在工作区用 -p <crate> 选包",
                );
                if value.is_none() {
                    arguments.next();
                }
            }
            "-pemp-app" => selects_app = true,
            "-p" | "--package" => {
                selects_app |= value.or_else(|| arguments.next()) == Some("emp-app");
            }
            "--test" => {
                if let Some(target) = value.or_else(|| arguments.next()) {
                    if target.contains("${{") {
                        report(
                            offset,
                            "--test 的值不能使用表达式；请把测试目标静态写进命令",
                        );
                    } else if is_test {
                        targets.insert(target);
                    }
                }
            }
            _ => {}
        }
    }
    if selects_app {
        for target in targets
            .into_iter()
            .filter(|target| !app_tests.contains(*target))
        {
            report(offset, &format!("emp-app 没有名为 {target} 的集成测试目标"));
        }
    }
}
