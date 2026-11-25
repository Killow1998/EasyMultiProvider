use super::arch::TestTargets;
use crate::ci::{Packages, Profile, Targets};
use regex::Regex;
use std::collections::BTreeSet;
use std::sync::LazyLock;

fn valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub(super) fn check_references(
    path: &str,
    contents: &str,
    profiles: &[Profile],
    referenced: &mut BTreeSet<&'static str>,
    errors: &mut Vec<String>,
) {
    static CI_PROFILE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bxtask\s+ci(?:\s+([^\s;&|()<>]+)?|[;&|()<>]|$)")
            .expect("CI profile workflow pattern")
    });
    for (index, line) in contents.lines().enumerate() {
        let line_number = index + 1;
        let mut captures = CI_PROFILE.captures_iter(line).peekable();
        if line.contains('#') && captures.peek().is_some() {
            errors.push(format!(
                "[ci-profile-line] {path}:{line_number}: 调用 profile 的这一行不能写注释，注释请另起一行"
            ));
        }
        for capture in captures {
            let name = capture.get(1).map_or("", |name| name.as_str());
            if !valid_profile_name(name) {
                errors.push(format!(
                    "[ci-profiles] {path}:{line_number}: profile 名不加引号，使用 [a-z0-9-]+"
                ));
                continue;
            }
            if let Some(profile) = profiles.iter().find(|profile| profile.name == name) {
                referenced.insert(profile.name);
            } else {
                errors.push(format!(
                    "[ci-profiles] {path}:{line_number}: 未知 CI profile {name}"
                ));
            }
        }
    }
}

pub(super) fn check_profiles(
    profiles: &[Profile],
    test_targets: &TestTargets,
    referenced: &BTreeSet<&str>,
    errors: &mut Vec<String>,
) {
    let mut names = BTreeSet::new();
    for profile in profiles {
        let location = format!("[ci-profiles] xtask/src/ci.rs: {}", profile.name);
        if !valid_profile_name(profile.name) {
            errors.push(format!("{location}: profile 名必须符合 [a-z0-9-]+"));
        }
        if !names.insert(profile.name) {
            errors.push(format!("{location}: profile 名重复"));
        }
        if profile.invocations.is_empty() {
            errors.push(format!("{location}: profile 至少需要一条测试调用"));
        }
        if !referenced.contains(profile.name) {
            errors.push(format!("{location}: 没有工作流引用此 profile"));
        }
        for invocation in profile.invocations {
            let targets = match invocation.targets {
                Targets::Tests(names) => names,
                _ => &[],
            };
            match invocation.packages {
                Packages::Selected(names) => {
                    for &package in names {
                        let Some(package_tests) = test_targets.get(package) else {
                            errors.push(format!("{location}: {package} 不是 workspace 成员"));
                            continue;
                        };
                        for &target in targets {
                            if !package_tests.contains(target) {
                                errors.push(format!(
                                    "{location}: {package} 没有名为 {target} 的集成测试目标"
                                ));
                            }
                        }
                    }
                }
                Packages::Workspace { .. } => {
                    for &target in targets {
                        if !test_targets.values().any(|tests| tests.contains(target)) {
                            errors.push(format!(
                                "{location}: workspace 没有名为 {target} 的集成测试目标"
                            ));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::Invocation;

    const HTTP_PROFILE: Profile = Profile {
        name: "verify-http",
        invocations: &[Invocation::new(
            Packages::Selected(&["emp-transport"]),
            Targets::Tests(&["http_client"]),
        )],
    };
    const PROFILES: &[Profile] = &[HTTP_PROFILE];

    #[test]
    fn rejects_empty_invocations() {
        let mut errors = Vec::new();
        let profiles = [Profile {
            invocations: &[],
            ..HTTP_PROFILE
        }];
        check_profiles(
            &profiles,
            &TestTargets::new(),
            &["verify-http"].into(),
            &mut errors,
        );
        assert_eq!(
            errors,
            ["[ci-profiles] xtask/src/ci.rs: verify-http: profile 至少需要一条测试调用"]
        );
    }

    #[test]
    fn rejects_duplicate_profile_names() {
        let mut errors = Vec::new();
        let targets = [("emp-transport".into(), ["http_client".into()].into())].into();
        check_profiles(
            &[HTTP_PROFILE, HTTP_PROFILE],
            &targets,
            &["verify-http"].into(),
            &mut errors,
        );
        assert_eq!(
            errors,
            ["[ci-profiles] xtask/src/ci.rs: verify-http: profile 名重复"]
        );
    }

    #[test]
    fn rejects_invalid_profile_names() {
        let mut errors = Vec::new();
        let targets = [("emp-transport".into(), ["http_client".into()].into())].into();
        let profiles = [Profile {
            name: "Verify_HTTP",
            ..HTTP_PROFILE
        }];
        check_profiles(&profiles, &targets, &["Verify_HTTP"].into(), &mut errors);
        assert_eq!(
            errors,
            ["[ci-profiles] xtask/src/ci.rs: Verify_HTTP: profile 名必须符合 [a-z0-9-]+"]
        );
    }

    #[test]
    fn requires_bare_workflow_profile_names() {
        let mut errors = Vec::new();
        let mut referenced = BTreeSet::new();
        check_references(
            "workflow.yml",
            "run: cargo xtask ci \"native-tls\"\nrun: cargo xtask ci native-memory",
            crate::ci::PROFILES,
            &mut referenced,
            &mut errors,
        );
        assert_eq!(referenced, ["native-memory"].into());
        assert_eq!(
            errors,
            ["[ci-profiles] workflow.yml:1: profile 名不加引号，使用 [a-z0-9-]+"]
        );
    }

    #[test]
    fn rejects_hash_on_profile_lines_without_losing_references() {
        for (command, expect_error) in [
            ("cargo xtask ci native-tls # x", true),
            ("sed 's|#.*||' a && cargo xtask ci native-tls", true),
            ("cargo xtask ci native-tls", false),
        ] {
            let mut errors = Vec::new();
            let mut referenced = BTreeSet::new();
            check_references(
                "workflow.yml",
                command,
                crate::ci::PROFILES,
                &mut referenced,
                &mut errors,
            );
            assert_eq!(referenced, ["native-tls"].into(), "{command}");
            if expect_error {
                assert_eq!(
                    errors,
                    [
                        "[ci-profile-line] workflow.yml:1: 调用 profile 的这一行不能写注释，注释请另起一行"
                    ],
                    "{command}"
                );
            } else {
                assert!(errors.is_empty(), "{command}: {errors:?}");
            }
        }
    }

    #[test]
    fn rejects_package_outside_workspace() {
        let mut errors = Vec::new();
        check_profiles(
            PROFILES,
            &TestTargets::new(),
            &["verify-http"].into(),
            &mut errors,
        );
        assert_eq!(
            errors,
            ["[ci-profiles] xtask/src/ci.rs: verify-http: emp-transport 不是 workspace 成员"]
        );
    }

    #[test]
    fn rejects_missing_test_target() {
        let mut errors = Vec::new();
        let targets = [("emp-transport".into(), BTreeSet::new())].into();
        check_profiles(PROFILES, &targets, &["verify-http"].into(), &mut errors);
        assert_eq!(
            errors,
            [
                "[ci-profiles] xtask/src/ci.rs: verify-http: emp-transport 没有名为 http_client 的集成测试目标"
            ]
        );
    }

    #[test]
    fn rejects_unknown_workflow_reference() {
        let mut errors = Vec::new();
        let mut referenced = BTreeSet::new();
        check_references(
            "workflow.yml",
            "run: cargo +1.93.1 xtask ci verify-http\nrun: cargo xtask ci missing",
            PROFILES,
            &mut referenced,
            &mut errors,
        );
        assert_eq!(referenced, ["verify-http"].into());
        assert_eq!(
            errors,
            ["[ci-profiles] workflow.yml:2: 未知 CI profile missing"]
        );
    }

    #[test]
    fn rejects_unreferenced_profile() {
        let mut errors = Vec::new();
        let targets = [("emp-transport".into(), ["http_client".into()].into())].into();
        check_profiles(PROFILES, &targets, &BTreeSet::new(), &mut errors);
        assert_eq!(
            errors,
            ["[ci-profiles] xtask/src/ci.rs: verify-http: 没有工作流引用此 profile"]
        );
    }
}
