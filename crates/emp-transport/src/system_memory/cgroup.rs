//! Bound host memory by the process's cgroup, including ancestor limits.
use super::{MemoryStatus, status_from_bytes};
use std::path::{Path, PathBuf};

pub(super) fn constrain(host: MemoryStatus) -> MemoryStatus {
    let (Ok(groups), Ok(mounts)) = (
        std::fs::read_to_string("/proc/self/cgroup"),
        std::fs::read_to_string("/proc/self/mountinfo"),
    ) else {
        return host;
    };
    let mut status = host;
    for line in groups.lines() {
        let mut parts = line.splitn(3, ':');
        let (_, Some(controllers), Some(group)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let v2 = controllers.is_empty();
        if !v2 && !controllers.split(',').any(|value| value == "memory") {
            continue;
        }
        for mount in mounts.lines() {
            let Some((left, right)) = mount.split_once(" - ") else {
                continue;
            };
            let fields: Vec<_> = left.split_ascii_whitespace().collect();
            let filesystem: Vec<_> = right.split_ascii_whitespace().collect();
            if fields.len() < 5 || filesystem.len() < 3 {
                continue;
            }
            if if v2 {
                filesystem[0] != "cgroup2"
            } else {
                filesystem[0] != "cgroup"
                    || !filesystem[2].split(',').any(|value| value == "memory")
            } {
                continue;
            }
            let root = PathBuf::from(unescape(fields[4]));
            let hierarchy = PathBuf::from(unescape(fields[3]));
            let relative = Path::new(group).strip_prefix(&hierarchy).ok();
            let directory = relative
                .filter(|path| {
                    path.components()
                        .all(|part| matches!(part, std::path::Component::Normal(_)))
                })
                .map(|path| root.join(path))
                .filter(|path| path.is_dir())
                .unwrap_or_else(|| root.clone());
            status = constrain_tree(status, &directory, &root, v2, number);
        }
    }
    status
}

fn unescape(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn constrain_tree(
    mut host: MemoryStatus,
    directory: &Path,
    root: &Path,
    v2: bool,
    read: impl Fn(&Path) -> Option<u64>,
) -> MemoryStatus {
    for ancestor in directory
        .ancestors()
        .take_while(|path| path.starts_with(root))
    {
        let usage = read(&ancestor.join(if v2 {
            "memory.current"
        } else {
            "memory.usage_in_bytes"
        }));
        let Some(usage) = usage else {
            continue;
        };
        for name in if v2 {
            &["memory.max", "memory.high"][..]
        } else {
            &["memory.limit_in_bytes"][..]
        } {
            let Some(limit) = read(&ancestor.join(name)) else {
                continue;
            };
            if limit == 0 {
                return MemoryStatus {
                    available: 0,
                    total: Some(0),
                    used: Some(0),
                    used_percent: Some(100.0),
                };
            }
            let total = (host.total.unwrap_or(usize::MAX) as u64).min(limit);
            let available = (host.available as u64)
                .min(limit.saturating_sub(usage))
                .min(total);
            if let Some(constrained) = status_from_bytes(total, available) {
                host = constrained;
            }
        }
    }
    host
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_and_ancestor_limits_both_bound_available_memory() {
        let root = Path::new("/fixture");
        let child = root.join("child");
        let mut values = std::collections::BTreeMap::from([
            (root.join("memory.max"), 1024),
            (root.join("memory.current"), 924),
            (child.join("memory.max"), 512),
            (child.join("memory.current"), 64),
        ]);
        let status = constrain_tree(
            status_from_bytes(8192, 4096).unwrap(),
            &child,
            root,
            true,
            |path| values.get(path).copied(),
        );
        assert_eq!(status.total, Some(512));
        assert_eq!(status.available, 100);
        values.insert(child.join("memory.current"), 600);
        assert_eq!(
            constrain_tree(status, &child, root, true, |path| values.get(path).copied()).available,
            0
        );
        values.insert(child.join("memory.max"), 0);
        let blocked = constrain_tree(status, &child, root, true, |path| values.get(path).copied());
        assert_eq!((blocked.total, blocked.available), (Some(0), 0));
    }
}
