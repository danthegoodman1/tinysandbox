//! Sessions, pipelines, redirects, and host-side access to the results.
//!
//! Run with: cargo run --example quickstart

use tinysandbox::sandbox::Sandbox;

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder().persist_session(true).build();

    // This example opts into a persistent shell session across exec calls.
    sandbox
        .exec("mkdir -p /workspace/logs && cd /workspace")
        .await;
    sandbox
        .exec("echo 'error: disk full\ninfo: started\nerror: disk full' > logs/app.log")
        .await;

    let result = sandbox.exec("grep -c error logs/app.log").await;
    println!("error lines: {}", result.stdout.trim());
    assert_eq!(result.stdout, "2\n");

    // Pipelines, && chains, and $? behave like bash.
    let result = sandbox
        .exec("sort -u logs/app.log | wc -l && echo exit=$?")
        .await;
    print!("{}", result.stdout);

    // Redirects write into the VFS; the host can read them back directly.
    sandbox.exec("grep error logs/app.log > errors.txt").await;
    let result = sandbox.exec("cat /workspace/errors.txt").await;
    print!("{}", result.stdout);

    // Every exec reports metrics.
    println!(
        "last exec: {:?} across {} command(s)",
        result.metrics.wall_time,
        result.metrics.commands.len()
    );

    // Sandbox-wide stats: VFS usage and total commands run.
    let stats = sandbox.stats();
    println!("stats: {stats:?}");
}
