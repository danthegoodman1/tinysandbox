use std::time::Duration;

use tinysandbox::sandbox::{Limits, Sandbox};

#[tokio::test]
async fn intermediate_allocations_fail_inside_the_guest_and_the_sandbox_recovers() {
    let memory_limit = 32 * 1024 * 1024;
    let sandbox = Sandbox::builder()
        .limits(Limits {
            jq_memory_bytes: memory_limit,
            wall_time: Duration::from_secs(3),
            ..Limits::default()
        })
        .build();
    // Output would be just a number. The intermediate string used to allocate
    // in the host despite the input/output byte caps; now even its allocation
    // request must pass through the guest memory limiter.
    let result = sandbox.exec("jq -n '\"x\" * 1000000000 | length'").await;
    assert_eq!(result.exit_code, 5, "{}", result.stderr);
    assert!(
        result.stderr.contains("memory limit exceeded"),
        "{}",
        result.stderr
    );
    assert!(result.stdout.is_empty());
    assert!(result.metrics.peak_wasm_memory_bytes.unwrap() <= memory_limit);

    let recovered = sandbox.exec("jq -nc '{ok: true}'").await;
    assert_eq!(recovered.exit_code, 0, "{}", recovered.stderr);
    assert_eq!(recovered.stdout, "{\"ok\":true}\n");
}

#[tokio::test]
async fn guest_initial_memory_is_also_subject_to_the_cap() {
    let sandbox = Sandbox::builder()
        .limits(Limits {
            jq_memory_bytes: 1,
            ..Limits::default()
        })
        .build();
    let result = sandbox.exec("jq -n '1'").await;
    assert_eq!(result.exit_code, 5, "{}", result.stderr);
    assert!(
        result.stderr.contains("memory limit exceeded"),
        "{}",
        result.stderr
    );
    assert_eq!(result.metrics.peak_wasm_memory_bytes, Some(0));
}

#[tokio::test]
async fn variable_json_and_filter_errors_keep_their_cli_exit_codes() {
    let sandbox = Sandbox::builder().build();
    let variable = sandbox.exec("jq -n --argjson value '{' '$value'").await;
    assert_eq!(variable.exit_code, 2, "{}", variable.stderr);
    assert!(variable.stderr.contains("invalid JSON for --argjson value"));
    let filter = sandbox.exec("jq -n '.['").await;
    assert_eq!(filter.exit_code, 3, "{}", filter.stderr);
}
