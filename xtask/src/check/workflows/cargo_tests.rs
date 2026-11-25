use super::{
    EXPRESSION, expression_pattern,
    yaml::{Node, NodeExt as _},
};
use regex::Regex;
use std::{borrow::Cow, collections::BTreeSet, sync::LazyLock};

pub(super) fn missing(
    script: &str,
    directory: &str,
    matrix: Option<&Node>,
    app_tests: &BTreeSet<String>,
) -> BTreeSet<String> {
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
    let script = CONTINUATION.replace_all(script, "");
    let mut command = Vec::new();
    let mut missing = BTreeSet::new();
    for token in TOKEN
        .find_iter(&script)
        .map(|token| token.as_str())
        .chain(["\n"])
    {
        match token.as_bytes()[0] {
            b'\n' | b';' | b'&' | b'|' => {
                for target in app_test_targets(&command, directory) {
                    missing.extend(
                        test_names(target, matrix)
                            .into_iter()
                            .filter(|name| !app_tests.contains(name.as_ref()))
                            .map(Cow::into_owned),
                    );
                }
                command.clear();
            }
            b'#' => {}
            _ => command.push(unquote(token)),
        }
    }
    missing
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

fn app_test_targets<'a>(command: &[&'a str], directory: &str) -> Vec<&'a str> {
    let mut arguments = command
        .iter()
        .copied()
        .skip_while(|argument| {
            argument.split_once('=').is_some_and(|(name, _)| {
                name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        })
        .peekable();
    if arguments.next() != Some("cargo") {
        return Vec::new();
    }
    if arguments
        .peek()
        .is_some_and(|argument| argument.starts_with('+'))
    {
        arguments.next();
    }
    if arguments.next() != Some("test") {
        return Vec::new();
    }
    let mut arguments = arguments.take_while(|argument| *argument != "--");
    let mut selects_app = false;
    let mut targets = Vec::new();
    while let Some(argument) = arguments.next() {
        let (option, value) = argument
            .split_once('=')
            .map_or((argument, None), |(option, value)| {
                (option, Some(unquote(value)))
            });
        match option {
            "-pemp-app" => selects_app = true,
            "-p" | "--package" => {
                selects_app |= value.or_else(|| arguments.next()) == Some("emp-app")
            }
            "--manifest-path" => {
                selects_app |= value
                    .or_else(|| arguments.next())
                    .is_some_and(|path| is_app_manifest(path, directory));
            }
            "--test" => targets.extend(value.or_else(|| arguments.next())),
            _ => {}
        }
    }
    if selects_app { targets } else { Vec::new() }
}

fn is_app_manifest(path: &str, directory: &str) -> bool {
    let from_workspace =
        path.starts_with(['/', '\\', '$']) || path.as_bytes().get(1) == Some(&b':');
    let base = if from_workspace { "" } else { directory };
    let mut components = Vec::new();
    for component in base.split(['/', '\\']).chain(path.split(['/', '\\'])) {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            _ => components.push(component),
        }
    }
    components.ends_with(&["crates", "emp-app", "Cargo.toml"])
}

fn test_names<'a>(target: &'a str, matrix: Option<&'a Node>) -> Vec<Cow<'a, str>> {
    let Some(expression) = EXPRESSION.captures(target) else {
        return vec![Cow::Borrowed(target)];
    };
    let (whole, [body]) = expression.extract::<1>();
    let Some((context, key)) = body.trim().split_once('.') else {
        return Vec::new();
    };
    if whole != target || !context.trim().eq_ignore_ascii_case("matrix") {
        return Vec::new();
    }
    let Some(matrix) = matrix else {
        return Vec::new();
    };
    let key = key.trim();
    let axis = matrix
        .get(key)
        .and_then(Node::as_sequence)
        .into_iter()
        .flatten();
    let included = matrix
        .get("include")
        .and_then(Node::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|row| row.get(key));
    axis.chain(included)
        .filter_map(Node::scalar_text)
        .filter(|name| !EXPRESSION.is_match(name))
        .collect()
}
