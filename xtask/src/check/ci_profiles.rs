use super::arch::TestTargets;
use crate::ci::{Packages, Profile, Targets};
use regex::Regex;
use std::collections::BTreeSet;
use std::sync::LazyLock;

pub(super) fn check_references(
    path: &str,
    contents: &str,
    profiles: &[Profile],
    referenced: &mut BTreeSet<&'static str>,
    errors: &mut Vec<String>,
) {
    static CI_PROFILE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\bxtask\s+ci\s+([^\s"';&|()<>]+)"#).expect("CI profile workflow pattern")
    });
    for (index, line) in contents.lines().enumerate() {
        let code = line.split_once('#').map_or(line, |(code, _)| code);
        for capture in CI_PROFILE.captures_iter(code) {
            let name = &capture[1];
            if let Some(profile) = profiles.iter().find(|profile| profile.name == name) {
                referenced.insert(profile.name);
            } else {
                errors.push(format!(
                    "[ci-profiles] {path}:{}: 未知 CI profile {name}",
                    index + 1
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
    for profile in profiles {
        let location = format!("[ci-profiles] xtask/src/ci.rs: {}", profile.name);
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

    const PROFILES: &[Profile] = &[Profile {
        name: "verify-http",
        invocations: &[Invocation::new(
            Packages::Selected(&["emp-transport"]),
            Targets::Tests(&["http_client"]),
        )],
    }];

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
