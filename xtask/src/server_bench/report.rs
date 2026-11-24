use crate::Result;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::Path;
use std::time::Duration;

const STARTUP_METRIC: &str = "Startup to /healthz 200 (ms)";

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StartupMeasurement {
    #[default]
    HealthProbeAfterAnnouncement,
    ConcurrentHealthProbe,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Report {
    pub(super) label: String,
    pub(super) startup_ms: f64,
    #[serde(default)]
    pub(super) startup_measurement: StartupMeasurement,
    pub(super) exit_ms: Option<f64>,
    pub(super) idle: Option<ProcessUsage>,
    pub(super) http: Vec<HttpRun>,
    pub(super) sse_requests: usize,
    pub(super) sse_first_byte_ms: Quantiles,
    pub(super) ws: WebSockets,
    pub(super) cancellation_within_1s: bool,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct HttpRun {
    pub(super) concurrency: usize,
    pub(super) requests: usize,
    pub(super) latency_ms: Quantiles,
    pub(super) requests_per_second: f64,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Quantiles {
    p50: f64,
    p99: f64,
}

impl Quantiles {
    pub(super) fn from_samples(mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        let nearest_rank =
            |percent: usize| milliseconds(samples[(samples.len() * percent).div_ceil(100) - 1]);
        Self {
            p50: nearest_rank(50),
            p99: nearest_rank(99),
        }
    }
}

pub(super) fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct WebSockets {
    pub(super) sessions: usize,
    pub(super) before: Option<ProcessUsage>,
    pub(super) open: Option<ProcessUsage>,
    pub(super) after: Option<ProcessUsage>,
    pub(super) after_15s: Option<ProcessUsage>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct ProcessUsage {
    rss_kib: u32,
    threads: u32,
}

impl ProcessUsage {
    fn values(&self) -> (f64, f64) {
        (f64::from(self.rss_kib) / 1024.0, f64::from(self.threads))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn read(pid: u32) -> io::Result<Option<Self>> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let number = |field: &str| -> io::Result<u32> {
            status
                .lines()
                .find_map(|line| line.strip_prefix(field))
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| io::Error::other(format!("missing {field} in /proc/{pid}/status")))
        };
        Ok(Some(Self {
            rss_kib: number("VmRSS:")?,
            threads: number("Threads:")?,
        }))
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn read(_pid: u32) -> io::Result<Option<Self>> {
        Ok(None)
    }
}

enum Metric {
    Number(f64),
    Check(bool),
    Unavailable,
}

impl Metric {
    fn text(&self) -> String {
        match self {
            Self::Number(value) => format!("{value:.3}"),
            Self::Check(true) => "yes".into(),
            Self::Check(false) => "no".into(),
            Self::Unavailable => "n/a".into(),
        }
    }

    fn change(&self, candidate: &Self) -> (String, String) {
        if let (Self::Number(old), Self::Number(new)) = (self, candidate) {
            let delta = new - old;
            let percent = if *old == 0.0 {
                "—".into()
            } else {
                format!("{:+.2}%", delta / old * 100.0)
            };
            (format!("{delta:+.3}"), percent)
        } else {
            ("—".into(), "—".into())
        }
    }
}

impl Report {
    pub(super) fn save(&self, path: &Path) -> Result {
        let write = || -> io::Result<()> {
            let mut file = File::create(path)?;
            serde_json::to_writer_pretty(&mut file, self)?;
            file.write_all(b"\n")
        };
        write().map_err(|error| format!("{}: {error}", path.display()))
    }

    pub(super) fn load(path: &Path) -> Result<Self> {
        let read = || -> io::Result<Self> {
            Ok(serde_json::from_reader(BufReader::new(File::open(path)?))?)
        };
        read().map_err(|error| format!("{}: {error}", path.display()))
    }

    fn metrics(&self) -> Vec<(String, Metric)> {
        let number = Metric::Number;
        let mut rows = vec![
            (STARTUP_METRIC.into(), number(self.startup_ms)),
            (
                "POST /api/quit to exit (ms)".into(),
                self.exit_ms.map_or(Metric::Unavailable, number),
            ),
        ];
        rows.extend(memory_metrics(
            "Idle",
            self.idle.as_ref().map(ProcessUsage::values),
        ));
        for run in &self.http {
            let description = format!("JSON c={} n={}", run.concurrency, run.requests);
            rows.extend([
                (
                    format!("{description} p50 (ms)"),
                    number(run.latency_ms.p50),
                ),
                (
                    format!("{description} p99 (ms)"),
                    number(run.latency_ms.p99),
                ),
                (
                    format!("{description} throughput (req/s)"),
                    number(run.requests_per_second),
                ),
            ]);
        }
        rows.extend([
            (
                format!("SSE first body byte c={} p50 (ms)", self.sse_requests),
                number(self.sse_first_byte_ms.p50),
            ),
            (
                format!("SSE first body byte c={} p99 (ms)", self.sse_requests),
                number(self.sse_first_byte_ms.p99),
            ),
        ]);
        let ws = format!("Responses WS n={}", self.ws.sessions);
        let before = self.ws.before.as_ref().map(ProcessUsage::values);
        rows.extend(memory_metrics(&format!("{ws} before"), before));
        for (phase, usage) in [
            ("open", &self.ws.open),
            ("closed +1 s", &self.ws.after),
            ("closed +15 s", &self.ws.after_15s),
        ] {
            let label = format!("{ws} {phase}");
            let values = usage.as_ref().map(ProcessUsage::values);
            rows.extend(memory_metrics(&label, values));
            let delta = values
                .zip(before)
                .map(|((rss, threads), (base_rss, base_threads))| {
                    (rss - base_rss, threads - base_threads)
                });
            rows.extend(memory_metrics(&format!("{label} vs before"), delta));
        }
        rows.push((
            "SSE upstream closed within 1 s of disconnect".into(),
            Metric::Check(self.cancellation_within_1s),
        ));
        rows
    }

    pub(super) fn table(&self) -> String {
        let label = table_label(&self.label);
        let mut table = format!("| Metric | {label} |\n|---|---:|\n");
        for (name, value) in self.metrics() {
            table.push_str(&format!("| {name} | {} |\n", value.text()));
        }
        table
    }

    pub(super) fn compare(&self, candidate: &Self) -> Result<String> {
        let left = self.metrics();
        let right = candidate.metrics();
        if !left
            .iter()
            .map(|row| &row.0)
            .eq(right.iter().map(|row| &row.0))
        {
            return Err("benchmark workloads differ".into());
        }
        let same_startup_measurement = self.startup_measurement == candidate.startup_measurement;
        let baseline = table_label(&self.label);
        let candidate = table_label(&candidate.label);
        let mut table = format!(
            "| Metric | {baseline} | {candidate} | Delta | Change |\n|---|---:|---:|---:|---:|\n"
        );
        for ((name, old), (_, new)) in left.into_iter().zip(right) {
            let (delta, change) = if name == STARTUP_METRIC && !same_startup_measurement {
                ("incomparable".into(), "incomparable".into())
            } else {
                old.change(&new)
            };
            table.push_str(&format!(
                "| {name} | {} | {} | {delta} | {change} |\n",
                old.text(),
                new.text()
            ));
        }
        Ok(table)
    }
}

fn memory_metrics(label: &str, usage: Option<(f64, f64)>) -> [(String, Metric); 2] {
    let (rss, threads) = usage.map_or(
        (Metric::Unavailable, Metric::Unavailable),
        |(rss, threads)| (Metric::Number(rss), Metric::Number(threads)),
    );
    [
        (format!("{label} RSS (MiB)"), rss),
        (format!("{label} threads"), threads),
    ]
}

fn table_label(label: &str) -> String {
    label.replace('|', "\\|").replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(label: &str, startup_ms: f64, cancellation: bool) -> Report {
        Report {
            label: label.into(),
            startup_ms,
            startup_measurement: StartupMeasurement::ConcurrentHealthProbe,
            exit_ms: Some(0.0),
            idle: Some(ProcessUsage {
                rss_kib: 17408,
                threads: 4,
            }),
            http: vec![HttpRun {
                concurrency: 4,
                requests: 2000,
                latency_ms: Quantiles {
                    p50: 1.25,
                    p99: 9.75,
                },
                requests_per_second: 300.0,
            }],
            sse_requests: 16,
            sse_first_byte_ms: Quantiles {
                p50: 2.5,
                p99: 11.0,
            },
            ws: WebSockets {
                sessions: 64,
                before: None,
                open: Some(ProcessUsage {
                    rss_kib: 23040,
                    threads: 70,
                }),
                after: Some(ProcessUsage {
                    rss_kib: 20480,
                    threads: 7,
                }),
                after_15s: None,
            },
            cancellation_within_1s: cancellation,
        }
    }

    #[test]
    fn nearest_rank_uses_sorted_samples_and_rounds_rank_up() {
        let samples = (1..=100).rev().map(Duration::from_millis).collect();
        assert_eq!(
            Quantiles::from_samples(samples),
            Quantiles {
                p50: 50.0,
                p99: 99.0
            }
        );
        let samples = [9, 1, 3].map(Duration::from_millis).to_vec();
        assert_eq!(
            Quantiles::from_samples(samples),
            Quantiles { p50: 3.0, p99: 9.0 }
        );
        assert_eq!(
            Quantiles::from_samples(vec![Duration::from_micros(125)]),
            Quantiles {
                p50: 0.125,
                p99: 0.125
            }
        );
    }

    #[test]
    fn json_round_trip_preserves_results_and_unsupported_samples() {
        let directory = crate::temporary_directory("emp-bench-report-").unwrap();
        let path = directory.path().join("report.json");
        let original = report("default", 12.6, true);
        original.save(&path).unwrap();
        let value: serde_json::Value = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        assert_eq!(value["startup_measurement"], "concurrent_health_probe");
        assert_eq!(value["ws"]["before"], serde_json::Value::Null);
        assert_eq!(value["http"][0]["latency_ms"]["p99"], 9.75);
        assert_eq!(Report::load(&path).unwrap(), original);
    }

    #[test]
    fn compares_candidate_minus_baseline_and_skips_undefined_changes() {
        let baseline = report("default", 20.0, false);
        let mut candidate = report("candidate|run", 15.0, true);
        candidate.exit_ms = Some(1.0);
        let comparison = baseline.compare(&candidate).unwrap();
        assert!(comparison.starts_with("| Metric | default | candidate\\|run | Delta | Change |"));
        assert!(
            comparison
                .contains("| Startup to /healthz 200 (ms) | 20.000 | 15.000 | -5.000 | -25.00% |")
        );
        assert!(
            comparison.contains("| POST /api/quit to exit (ms) | 0.000 | 1.000 | +1.000 | — |")
        );
        assert!(comparison.contains("| Responses WS n=64 before threads | n/a | n/a | — | — |"));
        assert!(
            comparison
                .contains("| SSE upstream closed within 1 s of disconnect | no | yes | — | — |")
        );
        candidate.http[0].requests = 1999;
        assert!(baseline.compare(&candidate).is_err());
    }

    #[test]
    fn compares_startup_only_when_bench_reports_use_the_same_measurement() {
        let h07: Report =
            serde_json::from_str(include_str!("fixtures/bench-h07-old-1.json")).unwrap();
        let pre_h08_json = include_str!("fixtures/bench-pre-h08-old-1.json");
        let pre_h08: Report = serde_json::from_str(pre_h08_json).unwrap();
        let comparison = h07.compare(&pre_h08).unwrap();
        assert!(
            comparison.contains(
                "| Startup to /healthz 200 (ms) | 32.207 | 20.213 | incomparable | incomparable |"
            ),
            "{comparison}"
        );
        assert!(
            comparison
                .contains("| POST /api/quit to exit (ms) | 19.177 | 30.129 | +10.952 | +57.11% |"),
            "{comparison}"
        );
        assert!(pre_h08.compare(&h07).unwrap().contains(
            "| Startup to /healthz 200 (ms) | 20.213 | 32.207 | incomparable | incomparable |"
        ));
        assert!(
            h07.compare(&h07)
                .unwrap()
                .contains("| Startup to /healthz 200 (ms) | 32.207 | 32.207 | +0.000 | +0.00% |")
        );

        let directory = crate::temporary_directory("emp-bench-report-").unwrap();
        let path = directory.path().join("report.json");
        Report {
            label: "new".into(),
            startup_ms: 20.0,
            startup_measurement: StartupMeasurement::ConcurrentHealthProbe,
            ..serde_json::from_str(pre_h08_json).unwrap()
        }
        .save(&path)
        .unwrap();
        let comparison = pre_h08.compare(&Report::load(&path).unwrap()).unwrap();
        assert!(
            comparison
                .contains("| Startup to /healthz 200 (ms) | 20.213 | 20.000 | -0.213 | -1.05% |"),
            "{comparison}"
        );
    }

    #[test]
    fn compares_ws_resource_changes_against_each_runs_pre_open_baseline() {
        let mut baseline = report("old", 20.0, true);
        baseline.ws.before = Some(ProcessUsage {
            rss_kib: 20480,
            threads: 4,
        });
        let mut candidate = report("candidate", 15.0, true);
        candidate.ws.before = Some(ProcessUsage {
            rss_kib: 23040,
            threads: 70,
        });
        candidate.ws.after_15s = Some(ProcessUsage {
            rss_kib: 22528,
            threads: 4,
        });
        candidate.exit_ms = None;
        let comparison = baseline.compare(&candidate).unwrap();
        for row in [
            "| POST /api/quit to exit (ms) | 0.000 | n/a | — | — |",
            "| Responses WS n=64 open vs before threads | 66.000 | 0.000 | -66.000 | -100.00% |",
            "| Responses WS n=64 closed +1 s vs before threads | 3.000 | -63.000 | -66.000 | -2200.00% |",
            "| Responses WS n=64 closed +15 s vs before threads | n/a | -66.000 | — | — |",
            "| Responses WS n=64 closed +15 s vs before RSS (MiB) | n/a | -0.500 | — | — |",
        ] {
            assert!(comparison.contains(row), "{comparison}");
        }
    }
}
