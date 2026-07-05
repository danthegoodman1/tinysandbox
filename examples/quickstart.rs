//! Sessions, pipelines, redirects, and host-side access to the results.
//!
//! Run with: cargo run --example quickstart

use thinbox::machine::Machine;

#[tokio::main]
async fn main() {
    let machine = Machine::builder().build();

    // cwd and env persist across execs, like a real shell session.
    machine
        .exec("mkdir -p /workspace/logs && cd /workspace")
        .await;
    machine
        .exec("echo 'error: disk full\ninfo: started\nerror: disk full' > logs/app.log")
        .await;

    let result = machine.exec("grep -c error logs/app.log").await;
    println!("error lines: {}", result.stdout.trim());
    assert_eq!(result.stdout, "2\n");

    // Pipelines, && chains, and $? behave like bash.
    let result = machine
        .exec("sort -u logs/app.log | wc -l && echo exit=$?")
        .await;
    print!("{}", result.stdout);

    // Redirects write into the VFS; the host can read them back directly.
    machine.exec("grep error logs/app.log > errors.txt").await;
    let result = machine.exec("cat /workspace/errors.txt").await;
    print!("{}", result.stdout);

    // Every exec reports metrics.
    println!(
        "last exec: {:?} across {} command(s)",
        result.metrics.wall_time,
        result.metrics.commands.len()
    );

    // Machine-wide stats: VFS usage and total commands run.
    let stats = machine.stats();
    println!("stats: {stats:?}");
}
