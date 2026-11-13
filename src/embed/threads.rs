//! How many ONNX Runtime threads the indexing embedder uses. Left alone, the runtime starts one
//! per core of the host, which in a container with a CPU limit means far more threads than the
//! limit lets run: they only throttle each other. So the count follows the limit: the cgroup v2
//! `cpu.max`, else the cgroup v1 CFS quota and period, else the cores the process may use,
//! rounded up and at least 1. `LORE_EMBED_THREADS` overrides all of it.

use std::path::Path;

const CGROUP_V2_CPU_MAX: &str = "/sys/fs/cgroup/cpu.max";
const CGROUP_V1_DIRS: [&str; 2] = ["/sys/fs/cgroup/cpu", "/sys/fs/cgroup/cpu,cpuacct"];

/// The CPUs `cpu.max` allows: `"max 100000"` is no limit (`None`), `"400000 100000"` is 4.
/// The period is optional (the kernel default is 100000). Anything else is `None`.
pub fn parse_cpu_max(text: &str) -> Option<f64> {
    let mut fields = text.split_whitespace();
    let quota = fields.next()?;
    let period = fields.next().unwrap_or("100000");
    if fields.next().is_some() || quota == "max" {
        return None;
    }
    ratio(quota.parse().ok()?, period.parse().ok()?)
}

/// The CPUs a cgroup v1 quota and period (`cpu.cfs_quota_us`, `cpu.cfs_period_us`) allow. A
/// quota of -1 is no limit (`None`), and so is anything that is not a positive number.
pub fn parse_cfs(quota: &str, period: &str) -> Option<f64> {
    ratio(quota.trim().parse().ok()?, period.trim().parse().ok()?)
}

fn ratio(quota: i64, period: i64) -> Option<f64> {
    (quota > 0 && period > 0).then(|| quota as f64 / period as f64)
}

/// The container's CPU limit in CPUs, read from `cpu_max` (a cgroup v2 file) or else the CFS
/// files in one of `v1_dirs`. `None` when there is no limit or nothing readable.
fn cpu_limit_in(cpu_max: &Path, v1_dirs: &[&Path]) -> Option<f64> {
    if let Ok(text) = std::fs::read_to_string(cpu_max) {
        return parse_cpu_max(&text);
    }
    v1_dirs.iter().find_map(|dir| {
        let quota = std::fs::read_to_string(dir.join("cpu.cfs_quota_us")).ok()?;
        let period = std::fs::read_to_string(dir.join("cpu.cfs_period_us")).ok()?;
        parse_cfs(&quota, &period)
    })
}

/// CPUs rounded up, and at least 1.
fn round_up(cpus: f64) -> usize {
    (cpus.ceil() as usize).max(1)
}

/// The thread count for `limit` (the container's CPU limit, if any) and `available` (the cores
/// the process may use).
pub fn threads_for(limit: Option<f64>, available: usize) -> usize {
    limit.map_or(available.max(1), round_up)
}

/// The ONNX thread count: `override_threads` (`LORE_EMBED_THREADS`) when set, else from the
/// container's CPU limit, else the available cores.
pub fn embed_threads(override_threads: Option<usize>) -> usize {
    if let Some(n) = override_threads.filter(|n| *n > 0) {
        return n;
    }
    let v1: Vec<&Path> = CGROUP_V1_DIRS.iter().map(Path::new).collect();
    let limit = cpu_limit_in(Path::new(CGROUP_V2_CPU_MAX), &v1);
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    threads_for(limit, available)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_max_is_quota_over_period() {
        assert_eq!(parse_cpu_max("400000 100000\n"), Some(4.0));
        assert_eq!(parse_cpu_max("150000 100000"), Some(1.5));
        assert_eq!(parse_cpu_max("50000 100000"), Some(0.5));
        assert_eq!(parse_cpu_max("200000"), Some(2.0));
    }

    #[test]
    fn cpu_max_without_a_limit_or_with_garbage_is_none() {
        assert_eq!(parse_cpu_max("max 100000\n"), None);
        assert_eq!(parse_cpu_max("max"), None);
        for bad in [
            "",
            "\n",
            "abc def",
            "400000 abc",
            "-1 100000",
            "0 100000",
            "100000 0",
            "1 2 3",
            "4e5 100000",
        ] {
            assert_eq!(parse_cpu_max(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn cfs_quota_of_minus_one_is_no_limit() {
        assert_eq!(parse_cfs("200000\n", "100000\n"), Some(2.0));
        assert_eq!(parse_cfs("-1\n", "100000\n"), None);
        assert_eq!(parse_cfs("x", "100000"), None);
        assert_eq!(parse_cfs("100000", "0"), None);
    }

    #[test]
    fn the_limit_is_rounded_up_and_at_least_one() {
        assert_eq!(threads_for(Some(0.25), 16), 1);
        assert_eq!(threads_for(Some(1.0), 16), 1);
        assert_eq!(threads_for(Some(1.5), 16), 2);
        assert_eq!(threads_for(Some(4.0), 16), 4);
        assert_eq!(threads_for(None, 8), 8);
        assert_eq!(threads_for(None, 0), 1);
    }

    #[test]
    fn the_override_wins_over_the_limit() {
        assert_eq!(embed_threads(Some(3)), 3);
        assert!(embed_threads(None) >= 1);
    }

    #[test]
    fn cgroup_files_are_read_v2_first_then_v1() {
        let dir = tempfile::tempdir().unwrap();
        let v2 = dir.path().join("cpu.max");
        let v1 = dir.path().join("v1");
        std::fs::create_dir(&v1).unwrap();
        std::fs::write(v1.join("cpu.cfs_quota_us"), "300000\n").unwrap();
        std::fs::write(v1.join("cpu.cfs_period_us"), "100000\n").unwrap();
        // No v2 file: the v1 files answer.
        assert_eq!(cpu_limit_in(&v2, &[v1.as_path()]), Some(3.0));
        // A v2 file says "max": no limit, and v1 is not consulted.
        std::fs::write(&v2, "max 100000\n").unwrap();
        assert_eq!(cpu_limit_in(&v2, &[v1.as_path()]), None);
        std::fs::write(&v2, "200000 100000\n").unwrap();
        assert_eq!(cpu_limit_in(&v2, &[v1.as_path()]), Some(2.0));
        // Nothing readable at all.
        assert_eq!(
            cpu_limit_in(
                &dir.path().join("nope"),
                &[dir.path().join("nada").as_path()]
            ),
            None
        );
    }
}
