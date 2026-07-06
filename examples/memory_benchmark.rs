use std::cmp;
use std::env;
use std::process::{Command, ExitCode};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tinysandbox::sandbox::Sandbox;

const DEFAULT_COUNTS: &[usize] = &[1_000, 10_000, 100_000, 1_000_000];
const DEFAULT_TASK_SAMPLE: usize = 1_000;
const TASK: &str = "mkdir -p /bench && echo bench-payload > /bench/echo.txt && cat /bench/echo.txt";
const TASK_OUTPUT: &str = "bench-payload\n";

#[derive(Debug)]
struct Config {
    counts: Vec<usize>,
    task_sample: usize,
    child_count: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Row {
    count: usize,
    baseline_rss_bytes: u64,
    active_rss_bytes: u64,
    active_delta_bytes: u64,
    active_bytes_per_sandbox: f64,
    active_peak_rss_bytes: u64,
    create_ms: u128,
    task_sample: usize,
    task_peak_rss_bytes: u64,
    task_delta_bytes: u64,
    task_bytes_per_sandbox: f64,
    task_extrapolated_peak_rss_bytes: u64,
    task_ms: u128,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let config = match parse_args(env::args().skip(1)) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            eprintln!(
                "usage: cargo run --release --example memory_benchmark -- [--counts 1000,10000,100000,1000000] [--task-sample 1000]"
            );
            return ExitCode::from(2);
        }
    };

    if let Some(count) = config.child_count {
        match run_child(count, config.task_sample).await {
            Ok(row) => {
                println!("{}", serde_json::to_string(&row).expect("row serializes"));
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("{err}");
                ExitCode::FAILURE
            }
        }
    } else {
        match run_parent(&config).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err}");
                ExitCode::FAILURE
            }
        }
    }
}

async fn run_parent(config: &Config) -> Result<(), String> {
    let mut rows = Vec::new();
    let exe = env::current_exe().map_err(|err| format!("failed to find current exe: {err}"))?;

    for count in &config.counts {
        let output = Command::new(&exe)
            .arg("--child")
            .arg("--count")
            .arg(count.to_string())
            .arg("--task-sample")
            .arg(config.task_sample.to_string())
            .output()
            .map_err(|err| format!("failed to run child benchmark for {count}: {err}"))?;

        if !output.status.success() {
            return Err(format!(
                "child benchmark for {count} failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let row: Row = serde_json::from_str(stdout.trim()).map_err(|err| {
            format!(
                "failed to parse child benchmark output for {count}: {err}\n{}",
                stdout
            )
        })?;
        rows.push(row);
    }

    println!("tinysandbox Rust memory benchmark");
    println!("task: `{TASK}`");
    println!(
        "task sample: up to {} sandboxes per count; task peak is extrapolated from the sampled RSS delta",
        config.task_sample
    );
    println!();
    print_table(&rows);
    Ok(())
}

async fn run_child(count: usize, task_sample_limit: usize) -> Result<Row, String> {
    let baseline_rss_bytes =
        current_rss_bytes().ok_or_else(|| "failed to read baseline RSS".to_owned())?;
    let mut peak_rss_bytes = baseline_rss_bytes;
    let sample_every = sample_interval(count);
    let create_started = Instant::now();
    let mut sandboxes = Vec::with_capacity(count);

    for index in 0..count {
        sandboxes.push(Sandbox::builder().build());
        if should_sample(index + 1, count, sample_every) {
            peak_rss_bytes = cmp::max(peak_rss_bytes, current_rss_bytes().unwrap_or(0));
        }
    }

    let active_rss_bytes =
        current_rss_bytes().ok_or_else(|| "failed to read active RSS".to_owned())?;
    peak_rss_bytes = cmp::max(peak_rss_bytes, active_rss_bytes);
    let create_ms = create_started.elapsed().as_millis();
    let active_delta_bytes = active_rss_bytes.saturating_sub(baseline_rss_bytes);
    let active_bytes_per_sandbox = per_sandbox(active_delta_bytes, count);

    let task_sample = count.min(task_sample_limit);
    let task_started = Instant::now();
    let task_start_rss_bytes = active_rss_bytes;
    let mut task_peak_rss_bytes = task_start_rss_bytes;
    let task_sample_every = sample_interval(task_sample);

    for (index, sandbox) in sandboxes.iter().take(task_sample).enumerate() {
        let result = sandbox.exec(TASK).await;
        if result.exit_code != 0 || result.stdout != TASK_OUTPUT {
            return Err(format!(
                "task failed at sandbox {index}: exit={} stdout={:?} stderr={:?}",
                result.exit_code, result.stdout, result.stderr
            ));
        }
        if should_sample(index + 1, task_sample, task_sample_every) {
            task_peak_rss_bytes = cmp::max(task_peak_rss_bytes, current_rss_bytes().unwrap_or(0));
        }
    }

    task_peak_rss_bytes = cmp::max(
        task_peak_rss_bytes,
        current_rss_bytes().unwrap_or(task_peak_rss_bytes),
    );
    let task_delta_bytes = task_peak_rss_bytes.saturating_sub(task_start_rss_bytes);
    let task_bytes_per_sandbox = per_sandbox(task_delta_bytes, task_sample);
    let task_extrapolated_peak_rss_bytes =
        active_rss_bytes.saturating_add((task_bytes_per_sandbox * count as f64).round() as u64);
    let task_ms = task_started.elapsed().as_millis();

    Ok(Row {
        count,
        baseline_rss_bytes,
        active_rss_bytes,
        active_delta_bytes,
        active_bytes_per_sandbox,
        active_peak_rss_bytes: peak_rss_bytes,
        create_ms,
        task_sample,
        task_peak_rss_bytes,
        task_delta_bytes,
        task_bytes_per_sandbox,
        task_extrapolated_peak_rss_bytes,
        task_ms,
    })
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Config, String> {
    let mut counts = DEFAULT_COUNTS.to_vec();
    let mut task_sample = DEFAULT_TASK_SAMPLE;
    let mut child_count = None;
    let mut is_child = false;
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--counts" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--counts requires a comma-separated value".to_owned())?;
                counts = parse_counts(&value)?;
            }
            "--task-sample" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--task-sample requires a value".to_owned())?;
                task_sample = value
                    .parse()
                    .map_err(|_| format!("invalid --task-sample value: {value}"))?;
            }
            "--child" => is_child = true,
            "--count" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--count requires a value".to_owned())?;
                child_count = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid --count value: {value}"))?,
                );
            }
            "-h" | "--help" => {
                return Err("memory benchmark for active tinysandbox instances".to_owned());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if is_child && child_count.is_none() {
        return Err("--child requires --count".to_owned());
    }
    if counts.is_empty() {
        return Err("--counts must include at least one count".to_owned());
    }
    if task_sample == 0 {
        return Err("--task-sample must be greater than zero".to_owned());
    }

    Ok(Config {
        counts,
        task_sample,
        child_count,
    })
}

fn parse_counts(value: &str) -> Result<Vec<usize>, String> {
    value
        .split(',')
        .map(|part| {
            let trimmed = part.trim();
            trimmed
                .parse()
                .map_err(|_| format!("invalid sandbox count: {trimmed}"))
        })
        .collect()
}

fn current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
                    return Some(kb.saturating_mul(1024));
                }
            }
        }
    }

    rss_from_ps()
}

fn rss_from_ps() -> Option<u64> {
    let output = Command::new("ps")
        .arg("-o")
        .arg("rss=")
        .arg("-p")
        .arg(std::process::id().to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let kb = stdout.trim().parse::<u64>().ok()?;
    Some(kb.saturating_mul(1024))
}

fn sample_interval(count: usize) -> usize {
    cmp::max(1, count / 20)
}

fn should_sample(done: usize, total: usize, interval: usize) -> bool {
    done == total || done.is_multiple_of(interval)
}

fn per_sandbox(bytes: u64, count: usize) -> f64 {
    if count == 0 {
        0.0
    } else {
        bytes as f64 / count as f64
    }
}

fn print_table(rows: &[Row]) {
    println!(
        "| active sandboxes | active peak RSS | active delta / sandbox | create time | task sample | measured task peak | extrapolated task peak | task time |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|");
    for row in rows {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            format_count(row.count),
            format_bytes(row.active_peak_rss_bytes),
            format_bytes(row.active_bytes_per_sandbox.round() as u64),
            format_duration_ms(row.create_ms),
            format_count(row.task_sample),
            format_bytes(row.task_peak_rss_bytes),
            format_bytes(row.task_extrapolated_peak_rss_bytes),
            format_duration_ms(row.task_ms),
        );
    }
}

fn format_count(value: usize) -> String {
    value
        .to_string()
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).expect("digits are utf8"))
        .collect::<Vec<_>>()
        .join(",")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn format_duration_ms(ms: u128) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", ms as f64 / 1_000.0)
    }
}
