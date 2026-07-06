#![cfg(feature = "js")]

// Node compatibility expectations in this file were regenerated with Node v24.15.0.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tinysandbox::sandbox::{FetchRequest, FetchResponse, Limits, Sandbox, SyscallError};
use tinysandbox::vfs::{InMemoryVfs, OpenMode, Vfs, VfsQuota};

#[tokio::test]
async fn js_eval_console_process_and_node_verified_shape() {
    // The console/process subset used here was checked against Node:
    // multiple console args are space-joined, argv carries user args, and env
    // values are visible through process.env.
    let sandbox = Sandbox::builder().env("TOKEN", "abc").build();
    let result = sandbox
        .exec("js -e 'console.log(\"hello\", { token: process.env.TOKEN }); console.error(process.argv[2]); process.exit(3)' arg")
        .await;

    assert_eq!(result.exit_code, 3);
    assert_eq!(result.stdout, "hello { token: 'abc' }\n");
    assert_eq!(result.stderr, "arg\n");
    assert!(result.metrics.peak_wasm_memory_bytes.unwrap_or_default() > 0);
}

#[tokio::test]
async fn js_usage_errors_report_message_and_status() {
    // Node has no `js` wrapper, so these pin the tinysandbox CLI contract for the
    // reviewer-requested wrapper failures.
    let sandbox = Sandbox::builder().build();

    let bare = sandbox.exec("js").await;
    assert_eq!(bare.exit_code, 1);
    assert_eq!(bare.stderr, "js: usage: js [-e code] script.js [args...]\n");

    let missing_eval_arg = sandbox.exec("js -e").await;
    assert_eq!(missing_eval_arg.exit_code, 1);
    assert_eq!(
        missing_eval_arg.stderr,
        "js: option requires an argument -- e\n"
    );

    let missing_script = sandbox.exec("js missing.js").await;
    assert_eq!(missing_script.exit_code, 1);
    assert_eq!(
        missing_script.stderr,
        "js: missing.js: no such file or directory\n"
    );
}

#[tokio::test]
async fn js_syscall_is_callable_from_guest() {
    // Verifies the generated `sandbox.<name>` binding calls the host handler and
    // returns its JSON value to the script.
    let sandbox = Sandbox::builder()
        .syscall("echo", |args| async move {
            Ok(json!({ "seen": args["value"].clone(), "ok": true }))
        })
        .build();

    let result = sandbox
        .exec(
            "js -e 'const out = sandbox.echo({ value: \"hello\" }); console.log(out.seen, out.ok)'",
        )
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "hello true\n");
}

#[tokio::test]
async fn js_syscall_json_values_round_trip_faithfully() {
    // Nested objects, arrays, unicode, nulls, booleans, and numeric values must
    // survive both guest-to-host and host-to-guest serialization.
    let sandbox = Sandbox::builder()
        .syscall("roundTrip", |args| async move {
            Ok(json!({
                "received": args,
                "unicode": "hello λ 🙂",
                "numbers": [1, 2.5, -3],
                "nested": { "ok": true, "none": null },
            }))
        })
        .build();
    let script = r#"
const value = sandbox.roundTrip({
  text: 'hi λ 🙂',
  list: [1, 2.5, { deep: true }],
  nothing: null
})
console.log(JSON.stringify(value))
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    let value: Value = serde_json::from_str(result.stdout.trim()).expect("valid JSON stdout");
    assert_eq!(
        value,
        json!({
            "received": {
                "text": "hi λ 🙂",
                "list": [1, 2.5, { "deep": true }],
                "nothing": null,
            },
            "unicode": "hello λ 🙂",
            "numbers": [1, 2.5, -3],
            "nested": { "ok": true, "none": null },
        })
    );
}

#[tokio::test]
async fn js_syscall_arguments_preserve_undefined_and_scalars() {
    // The glue maps an omitted argument to JSON null while preserving scalar
    // argument values without wrapping them in an object.
    let sandbox = Sandbox::builder()
        .syscall("echo", |args| async move { Ok(args) })
        .build();
    let script = r#"
console.log(JSON.stringify([
  sandbox.echo(),
  sandbox.echo(42),
  sandbox.echo('s')
]))
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    let value: Value = serde_json::from_str(result.stdout.trim()).expect("valid JSON stdout");
    assert_eq!(value, json!([null, 42, "s"]));
}

#[tokio::test]
async fn js_syscall_error_is_catchable_with_code() {
    // Handler errors become ordinary guest Error objects with message and
    // optional code fields.
    let sandbox = Sandbox::builder()
        .syscall("fail", |_args| async {
            Err(SyscallError::new("access denied").with_code("E_DENIED"))
        })
        .build();
    let script = r#"
try {
  sandbox.fail({ id: 1 })
} catch (err) {
  console.log(err.message)
  console.log(err.code)
}
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "access denied\nE_DENIED\n");
}

#[tokio::test]
async fn js_syscall_handlers_use_embedder_runtime_and_wall_timeout() {
    // A timer-backed handler proves syscalls run on the embedder runtime; a
    // pending handler must return a guest-visible timeout error.
    let timed = Sandbox::builder()
        .syscall("delay", |_args| async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok(json!("done"))
        })
        .build()
        .exec("js -e 'console.log(sandbox.delay())'")
        .await;
    assert_eq!(timed.exit_code, 0, "stderr: {}", timed.stderr);
    assert_eq!(timed.stdout, "done\n");

    let sandbox = Sandbox::builder()
        .limits(Limits {
            wall_time: Duration::from_millis(500),
            ..Limits::default()
        })
        .syscall("hang", |_args| async {
            std::future::pending::<Result<Value, SyscallError>>().await
        })
        .build();
    let start = Instant::now();
    let result = sandbox
        .exec("js -e 'try { sandbox.hang() } catch (err) { console.log(err.message) }'")
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(result.stdout, "syscall 'hang' timed out\n");
}

#[tokio::test]
async fn js_fetch_get_json_happy_path() {
    // Verifies the global fetch binding calls the host transport and exposes
    // status, status text, headers, and JSON body helpers.
    let seen = Arc::new(Mutex::new(Vec::<FetchRequest>::new()));
    let handler_seen = Arc::clone(&seen);
    let sandbox = Sandbox::builder()
        .fetch(move |request| {
            let handler_seen = Arc::clone(&handler_seen);
            async move {
                handler_seen
                    .lock()
                    .expect("record fetch request")
                    .push(request);
                Ok(FetchResponse {
                    status: 200,
                    headers: vec![
                        ("content-type".to_owned(), "application/json".to_owned()),
                        ("x-answer".to_owned(), "42".to_owned()),
                    ],
                    body: br#"{"ok":true}"#.to_vec(),
                })
            }
        })
        .build();
    let script = r#"
(async () => {
  const res = await fetch('https://example.test/data')
  console.log(res.ok, res.status, res.statusText, res.headers.get('Content-Type'))
  const data = await res.json()
  console.log(data.ok)
})()
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "true 200 OK application/json\ntrue\n");
    let seen = seen.lock().expect("fetch request recorded");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].url, "https://example.test/data");
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].body, None);
}

#[tokio::test]
async fn js_fetch_post_string_body_and_header_object() {
    // Header objects normalize to case-insensitive names and string bodies are
    // transported as UTF-8 bytes.
    let seen = Arc::new(Mutex::new(Vec::<FetchRequest>::new()));
    let handler_seen = Arc::clone(&seen);
    let sandbox = Sandbox::builder()
        .fetch(move |request| {
            let handler_seen = Arc::clone(&handler_seen);
            async move {
                handler_seen
                    .lock()
                    .expect("record fetch request")
                    .push(request);
                Ok(FetchResponse {
                    status: 204,
                    headers: Vec::new(),
                    body: Vec::new(),
                })
            }
        })
        .build();
    let script = r#"
(async () => {
  const res = await fetch('https://example.test/post', {
    method: 'post',
    headers: { 'Content-Type': 'text/plain', 'X-Token': 'abc' },
    body: 'hello'
  })
  console.log(res.status, res.ok)
})()
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "204 true\n");
    let seen = seen.lock().expect("fetch request recorded");
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].body.as_deref(), Some(b"hello".as_slice()));
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == "text/plain")
    );
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|(name, value)| name == "x-token" && value == "abc")
    );
}

#[tokio::test]
async fn js_fetch_binary_body_round_trips_bytes() {
    // Request and response bodies use base64 at the host boundary so binary
    // bytes survive without UTF-8 coercion.
    let seen = Arc::new(Mutex::new(Vec::<FetchRequest>::new()));
    let handler_seen = Arc::clone(&seen);
    let sandbox = Sandbox::builder()
        .fetch(move |request| {
            let handler_seen = Arc::clone(&handler_seen);
            async move {
                handler_seen
                    .lock()
                    .expect("record fetch request")
                    .push(request);
                Ok(FetchResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: vec![9, 8, 7, 0],
                })
            }
        })
        .build();
    let script = r#"
(async () => {
  const res = await fetch('https://example.test/binary', {
    method: 'POST',
    body: new Uint8Array([0, 255, 4, 128])
  })
  const bytes = new Uint8Array(await res.arrayBuffer())
  console.log(Array.from(bytes).join(','))
})()
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "9,8,7,0\n");
    let seen = seen.lock().expect("fetch request recorded");
    assert_eq!(seen[0].body.as_deref(), Some(&[0, 255, 4, 128][..]));
}

#[tokio::test]
async fn js_fetch_handler_error_rejects_with_fetch_failed_cause() {
    // Handler errors follow undici's catchable TypeError shape and preserve the
    // host error message on `cause`.
    let sandbox = Sandbox::builder()
        .fetch(|_request| async { Err(SyscallError::new("upstream unavailable")) })
        .build();
    let script = r#"
(async () => {
  try {
    await fetch('https://example.test/fail')
  } catch (err) {
    console.log(err.name, err.message)
    console.log(err.cause && err.cause.message)
  }
})()
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "TypeError fetch failed\nupstream unavailable\n"
    );
}

#[tokio::test]
async fn js_fetch_hanging_handler_rejects_before_command_timeout() {
    // A stuck handler should use the remaining wall-time budget to produce a
    // catchable fetch rejection before the outer command timeout wins.
    let sandbox = Sandbox::builder()
        .limits(Limits {
            wall_time: Duration::from_millis(500),
            ..Limits::default()
        })
        .fetch(|_request| async {
            std::future::pending::<Result<FetchResponse, SyscallError>>().await
        })
        .build();
    let start = Instant::now();
    let script = r#"
(async () => {
  try {
    await fetch('https://example.test/hang')
  } catch (err) {
    console.log(err.name, err.message)
    console.log(err.cause && err.cause.message)
  }
})()
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(result.stdout, "TypeError fetch failed\nfetch timed out\n");
}

#[tokio::test]
async fn js_fetch_without_handler_rejects_but_global_exists() {
    // The fetch global is always present, while network access remains disabled
    // until the embedder provides a handler.
    let sandbox = Sandbox::builder().build();
    let script = r#"
(async () => {
  console.log(typeof fetch)
  try {
    await fetch('https://example.test/')
  } catch (err) {
    console.log(err.name, err.message)
    console.log(err.cause && err.cause.message)
  }
})()
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert!(
        result
            .stdout
            .starts_with("function\nTypeError fetch failed\n")
    );
    assert!(result.stdout.contains("network is not available"));
}

#[tokio::test]
async fn js_fetch_double_body_read_matches_undici_message() {
    // Response body helpers consume exactly once and reject with undici's
    // TypeError message on a second read.
    let sandbox = Sandbox::builder()
        .fetch(|_request| async {
            Ok(FetchResponse {
                status: 200,
                headers: Vec::new(),
                body: b"hello".to_vec(),
            })
        })
        .build();
    let script = r#"
(async () => {
  const res = await fetch('https://example.test/body')
  console.log(await res.text())
  try {
    await res.text()
  } catch (err) {
    console.log(err.name, err.message)
  }
})()
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "hello\nTypeError Body is unusable: Body has already been read\n"
    );
}

#[tokio::test]
async fn js_fetch_headers_are_case_insensitive_and_iterable() {
    // Duplicate names combine with comma-space, lookup is case-insensitive, and
    // iteration exposes normalized header names.
    let sandbox = Sandbox::builder().build();
    let script = r#"
const headers = new Headers([
  ['X-Test', 'one'],
  ['x-test', 'two'],
  ['Content-Type', 'text/plain']
])
console.log(headers.get('X-TEST'))
console.log(headers.has('content-type'))
const forEach = []
headers.forEach((value, name) => forEach.push(`${name}=${value}`))
console.log(forEach.join('|'))
console.log(Array.from(headers.entries()).map(([name, value]) => `${name}:${value}`).join('|'))
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "one, two\ntrue\ncontent-type=text/plain|x-test=one, two\ncontent-type:text/plain|x-test:one, two\n"
    );
}

#[tokio::test]
async fn js_fetch_signal_is_loudly_unsupported() {
    // AbortController is outside this phase's subset, so passing a signal
    // rejects clearly instead of pretending cancellation works.
    let sandbox = Sandbox::builder().build();
    let script = r#"
(async () => {
  try {
    await fetch('https://example.test/', { signal: {} })
  } catch (err) {
    console.log(err.name)
    console.log(err.message)
  }
})()
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "TypeError\nAbortSignal is not supported in tinysandbox fetch\n"
    );
}

#[tokio::test]
async fn js_fetch_response_size_cap_rejects_with_custom_limit() {
    // The host enforces the configured response cap before base64 response
    // bytes are returned to the guest.
    let sandbox = Sandbox::builder()
        .limits(Limits {
            fetch_response_bytes: 3,
            ..Limits::default()
        })
        .fetch(|_request| async {
            Ok(FetchResponse {
                status: 200,
                headers: Vec::new(),
                body: vec![1, 2, 3, 4],
            })
        })
        .build();
    let script = r#"
(async () => {
  try {
    await fetch('https://example.test/too-large')
  } catch (err) {
    console.log(err.name, err.message)
    console.log(err.cause && err.cause.message)
  }
})()
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "TypeError fetch failed\nfetch response body exceeded limit of 3 bytes\n"
    );
}

#[tokio::test]
async fn js_microtasks_are_drained_without_fetch() {
    // Already-settled promise callbacks must run before the JS command exits.
    let sandbox = Sandbox::builder().build();
    let result = sandbox
        .exec("js -e 'Promise.resolve().then(() => console.log(\"drained\"))'")
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "drained\n");
}

#[tokio::test]
async fn js_unhandled_top_level_rejection_exits_nonzero() {
    // A rejected promise that reaches the drain unhandled is reported like an
    // uncaught error.
    let sandbox = Sandbox::builder().build();
    let result = sandbox
        .exec("js -e 'Promise.reject(new Error(\"async boom\"))'")
        .await;

    assert_ne!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.starts_with("Error: async boom\n"));
}

#[tokio::test]
async fn js_unhandled_rejection_tracker_keeps_unhandled_first_reason() {
    // Handling a later rejection must not clear an earlier unhandled promise.
    let sandbox = Sandbox::builder().build();
    let script = r#"
const a = Promise.reject(new Error("A"))
const b = Promise.reject(new Error("B"))
b.catch(() => {})
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_ne!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.starts_with("Error: A\n"));
}

#[tokio::test]
async fn js_unhandled_rejection_tracker_reports_second_if_first_later_handled() {
    // A second rejected promise remains reportable even if the earlier one is
    // handled before the drain completes.
    let sandbox = Sandbox::builder().build();
    let script = r#"
const first = Promise.reject(new Error("first"))
const second = Promise.reject(new Error("second"))
first.catch(() => {})
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_ne!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.starts_with("Error: second\n"));
}

#[tokio::test]
async fn js_unhandled_rejection_tracker_clears_every_eventually_handled_promise() {
    // Rejections handled from a later microtask should leave no stale entries at
    // the final drain.
    let sandbox = Sandbox::builder().build();
    let script = r#"
const a = Promise.reject(new Error("A"))
const b = Promise.reject(new Error("B"))
Promise.resolve().then(() => {
  a.catch(() => console.log("A handled"))
  b.catch(() => console.log("B handled"))
})
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "A handled\nB handled\n");
}

#[tokio::test]
async fn js_internal_host_globals_are_hidden_from_guest() {
    // The generated syscall namespace is visible, but the raw host ABI and
    // config object are not guest capabilities.
    let sandbox = Sandbox::builder()
        .syscall("known", |_args| async { Ok(Value::Null) })
        .build();
    let script = r#"
console.log(Object.keys(sandbox).join(','))
console.log([
  '__tinysandbox_host_call',
  '__tinysandbox_stdout',
  '__tinysandbox_stderr',
  '__tinysandbox_exit',
  '__tinysandbox_eval_module',
  '__tinysandboxConfig'
].map(name => typeof globalThis[name]).join(','))
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "known\nundefined,undefined,undefined,undefined,undefined,undefined\n"
    );
}

#[tokio::test]
async fn js_without_syscalls_has_no_sandbox_global() {
    // Keeps the baseline guest global surface unchanged when the embedder did
    // not opt in to any syscalls.
    let sandbox = Sandbox::builder().build();
    let result = sandbox.exec("js -e 'console.log(typeof sandbox)'").await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "undefined\n");
}

#[tokio::test]
async fn js_prelude_defines_global_visible_to_script() {
    // The prelude runs before user code in global scope.
    let sandbox = Sandbox::builder()
        .js_prelude("globalThis.answer = 42")
        .build();
    let result = sandbox.exec("js -e 'console.log(answer)'").await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "42\n");
}

#[tokio::test]
async fn js_prelude_can_wrap_syscall_and_delete_sandbox_global() {
    // A prelude can expose a narrower wrapper, remove the generated syscall
    // namespace, and still leave the raw host boundary hidden from the script.
    let sandbox = Sandbox::builder()
        .syscall("secret", |args| async move { Ok(json!({ "value": args["value"].clone() })) })
        .js_prelude(
            "const secret = sandbox.secret; globalThis.callSecret = value => secret({ value }).value; delete globalThis.sandbox",
        )
        .build();
    let result = sandbox
        .exec("js -e 'console.log(callSecret(\"ok\"), typeof sandbox, typeof __tinysandbox_host_call, typeof __tinysandboxConfig)'")
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "ok undefined undefined undefined\n");
}

#[tokio::test]
async fn js_prelude_throw_aborts_with_prelude_stack() {
    // Prelude failures abort before the script and attribute the stack to
    // `<prelude>`.
    let sandbox = Sandbox::builder()
        .js_prelude("throw new Error('prelude failed')")
        .build();
    let result = sandbox.exec("js -e 'console.log(\"script ran\")'").await;

    assert_ne!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    assert!(result.stderr.contains("prelude failed"));
    assert!(result.stderr.contains("<prelude>"), "{}", result.stderr);
}

#[test]
fn js_syscall_names_are_validated_at_build() {
    // Registry names become JavaScript property names generated by the host.
    for name in ["", "1bad", "bad-name"] {
        let result = std::panic::catch_unwind(|| {
            Sandbox::builder()
                .syscall(name, |_args| async { Ok(Value::Null) })
                .build()
        });
        assert!(result.is_err(), "{name:?} should be invalid");
    }

    let duplicate = std::panic::catch_unwind(|| {
        Sandbox::builder()
            .syscall("dup", |_args| async { Ok(Value::Null) })
            .syscall("dup", |_args| async { Ok(Value::Null) })
            .build()
    });
    assert!(duplicate.is_err(), "duplicate syscall names should panic");

    let reserved_fetch = std::panic::catch_unwind(|| {
        Sandbox::builder()
            .syscall("fetch", |_args| async { Ok(Value::Null) })
            .build()
    });
    assert!(
        reserved_fetch.is_err(),
        "reserved fetch syscall name should panic"
    );
}

#[tokio::test]
async fn js_eval_commonjs_entry_matches_node() {
    // Node v24.15.0 eval entries have no require.main, keep module.id as
    // [eval], and do not bind top-level this to module.exports.
    let sandbox = Sandbox::builder().build();
    let result = sandbox
        .exec("js -e 'console.log(require.main === undefined, require.main === module, module.id, this === module.exports)'")
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "true false [eval] false\n");
}

#[tokio::test]
async fn js_config_json_is_stable_across_allocator_alignment() {
    // Varies script length across and beyond a mod-16 allocator window so the
    // QuickJS JSON parser must rely on tinysandbox's explicit NUL sentinel.
    let sandbox = Sandbox::builder().env("TOKEN", "abc").build();

    for filler_len in 0..32 {
        let script = format!(
            "/*{}*/\nconsole.log(process.env.TOKEN)",
            "x".repeat(filler_len)
        );
        let result = sandbox
            .exec(&format!("js -e '{}'", shell_single_quote(&script)))
            .await;

        assert_eq!(
            result.exit_code, 0,
            "filler length {filler_len} failed with stderr: {}",
            result.stderr
        );
        assert_eq!(result.stdout, "abc\n");
        assert!(!result.stderr.contains("<tinysandbox-config>"));
    }
}

#[tokio::test]
async fn js_uncaught_errors_print_node_shaped_stacks() {
    // Node prints a type/message header followed by stack frames for uncaught
    // Error objects. QuickJS supplies frames separately, so tinysandbox composes the
    // same header shape before appending them.
    let sandbox = Sandbox::builder().build();

    let short = sandbox.exec("js -e 'throw new Error(\"x\")'").await;
    assert_eq!(short.exit_code, 1);
    assert!(short.stderr.starts_with("Error: x\n"));
    assert!(short.stderr.contains("    at "));

    let long = sandbox
        .exec("js -e 'throw new Error(\"boom boom boom boom\")'")
        .await;
    assert_eq!(long.exit_code, 1);
    assert!(long.stderr.starts_with("Error: boom boom boom boom\n"));
    assert!(long.stderr.contains("    at "));

    let type_error = sandbox.exec("js -e 'const f = undefined; f()'").await;
    assert_eq!(type_error.exit_code, 1);
    assert!(type_error.stderr.starts_with("TypeError:"));
    assert!(type_error.stderr.contains("not a function"));
    assert!(type_error.stderr.contains("    at "));
}

#[tokio::test]
async fn js_recursion_uses_catchable_quickjs_stack_limit() {
    // A deep but finite call chain should run, while unbounded recursion should
    // become a JavaScript exception rather than a wasmtime stack trap.
    let sandbox = Sandbox::builder().build();
    let legal_depth = r#"
function f(n) { return n === 0 ? 42 : f(n - 1) }
console.log(f(2000))
"#;
    let legal = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(legal_depth)))
        .await;
    assert_eq!(legal.exit_code, 0, "stderr: {}", legal.stderr);
    assert_eq!(legal.stdout, "42\n");

    let unbounded = sandbox
        .exec("js -e 'function f() { return f() }; f()'")
        .await;
    assert_eq!(unbounded.exit_code, 1);
    assert!(
        unbounded.stderr.contains("stack") || unbounded.stderr.contains("call stack"),
        "stderr: {}",
        unbounded.stderr
    );
    assert!(!unbounded.stderr.contains("wasm trap"));
    assert!(!unbounded.stderr.contains("wasm backtrace"));

    let caught = sandbox
        .exec("js -e 'function f() { return f() }; try { f() } catch (err) { console.log(\"caught\", /stack|call stack/i.test(String(err && err.message))) }'")
        .await;
    assert_eq!(caught.exit_code, 0, "stderr: {}", caught.stderr);
    assert_eq!(caught.stdout, "caught true\n");
}

#[tokio::test]
async fn js_process_exit_is_not_catchable() {
    // Node exits immediately here with the requested status and never reaches
    // catch, finally, or later statements.
    let sandbox = Sandbox::builder().build();
    let result = sandbox
        .exec("js -e 'try { process.exit(5) } catch (e) {} ; console.log(\"after\")'")
        .await;

    assert_eq!(result.exit_code, 5);
    assert_eq!(result.stdout, "");

    let finally = sandbox
        .exec("js -e 'try { process.exit(7) } finally { console.log(\"finally ran\") }'")
        .await;
    assert_eq!(finally.exit_code, 7);
    assert_eq!(finally.stdout, "");
}

#[tokio::test]
async fn js_fs_sync_surface_round_trips_text_binary_and_offsets() {
    // Exercises whole-file APIs and descriptor-position semantics. The final
    // file shape matches the same sequence under Node on a real filesystem.
    let sandbox = Sandbox::builder().build();
    let script = r#"
const fs = require('fs')
fs.mkdirSync('/work', { recursive: true })
fs.writeFileSync('/work/text.txt', 'hello')
fs.appendFileSync('/work/text.txt', ' world')
const fd = fs.openSync('/work/bin', 'w+')
fs.writeSync(fd, Buffer.from([1, 2, 3, 4]), 0, 4, 0)
fs.writeSync(fd, Buffer.from([9]), 0, 1, 2)
fs.ftruncateSync(fd, 3)
fs.closeSync(fd)
const input = Buffer.alloc(4)
const readFd = fs.openSync('/work/bin', 'r')
const n = fs.readSync(readFd, input, 1, 3, 0)
fs.closeSync(readFd)
console.log(fs.readFileSync('/work/text.txt', 'utf8'))
console.log(n, Array.from(input).join(','))
console.log(fs.readdirSync('/work').join(','))
const stat = fs.statSync('/work/bin')
console.log(stat.isFile(), stat.isDirectory(), stat.size)
"#;

    assert_eq!(
        sandbox
            .exec(&format!("js -e '{}'", shell_single_quote(script)))
            .await
            .stdout,
        "hello world\n3 0,1,2,9\nbin,text.txt\ntrue false 3\n"
    );
}

#[tokio::test]
async fn js_fs_read_lines_sync_iterates_text_lines() {
    // Covers the tinysandbox line helper's public contract: it is a sync
    // iterable, strips LF/CRLF separators, preserves blank lines, and does not
    // emit an extra line for a final newline.
    let sandbox = Sandbox::builder().build();
    let script = r#"
const fs = require('fs')
function dump(path, options) {
  console.log(JSON.stringify(Array.from(fs.readLinesSync(path, options))))
}
fs.writeFileSync('/normal', 'alpha\nbeta\n')
fs.writeFileSync('/mixed', 'a\r\nb\n\nc')
fs.writeFileSync('/unterminated', 'last')
fs.writeFileSync('/blank', '\n\nmiddle\n\n')
const iterator = fs.readLinesSync('/normal')
console.log(typeof iterator[Symbol.iterator], iterator === iterator[Symbol.iterator]())
const normal = []
for (const line of iterator) normal.push(line)
console.log(JSON.stringify(normal))
dump('/mixed', 'utf8')
dump('/unterminated', { encoding: 'utf-8' })
dump('/blank')
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "function true\n[\"alpha\",\"beta\"]\n[\"a\",\"b\",\"\",\"c\"]\n[\"last\"]\n[\"\",\"\",\"middle\",\"\"]\n"
    );
}

#[tokio::test]
async fn js_fs_read_lines_sync_closes_fd_after_iteration_stops() {
    // Unlinking a fully quota-sized file only frees its storage after the read
    // fd closes. The follow-up write would fail with ENOSPC if the iterator
    // leaked the descriptor on break, throw, or exhaustion.
    let sandbox = Sandbox::builder()
        .vfs(InMemoryVfs::new(VfsQuota {
            max_bytes: 4,
            max_files: 8,
            max_file_size: 4,
        }))
        .build();
    let script = r#"
const fs = require('fs')
function releaseAfter(label, consume) {
  fs.writeFileSync('/input', 'a\nb\n')
  consume(fs.readLinesSync('/input'))
  fs.unlinkSync('/input')
  fs.writeFileSync(`/${label}`, 'x')
  console.log(label, fs.readFileSync(`/${label}`, 'utf8'))
  fs.unlinkSync(`/${label}`)
}
releaseAfter('break', iter => {
  for (const line of iter) {
    console.log('seen', line)
    break
  }
})
releaseAfter('throw', iter => {
  try {
    for (const line of iter) {
      console.log('throw-seen', line)
      throw new Error('stop')
    }
  } catch (err) {
    console.log('caught', err.message)
  }
})
releaseAfter('done', iter => {
  console.log('done-lines', Array.from(iter).join('|'))
})
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "seen a\nbreak x\nthrow-seen a\ncaught stop\nthrow x\ndone-lines a|b\ndone x\n"
    );
}

#[tokio::test]
async fn js_fs_write_buffer_two_arg_form_writes_all_bytes() {
    // Node returns 5 and writes the full Buffer for writeSync(fd, buffer).
    let sandbox = Sandbox::builder().build();
    let script = r#"
const fs = require('fs')
fs.writeFileSync('/out', '')
const fd = fs.openSync('/out', 'r+')
const n = fs.writeSync(fd, Buffer.from('hello'))
fs.closeSync(fd)
console.log(n, fs.readFileSync('/out').toString())
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "5 hello\n");
}

#[tokio::test]
async fn js_fs_buffer_to_string_and_is_buffer_match_node() {
    // Node returns Buffer from readFileSync without encoding, decodes UTF-8 by
    // default, and does not treat a plain Uint8Array as a Buffer.
    let sandbox = Sandbox::builder().build();
    let script = r#"
const fs = require('fs')
fs.writeFileSync('/text', 'hello')
console.log(fs.readFileSync('/text').toString())
console.log(Buffer.from('hi').toString('utf8'))
console.log(Buffer.isBuffer(fs.readFileSync('/text')), Buffer.isBuffer(new Uint8Array()))
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "hello\nhi\ntrue false\n");
}

#[tokio::test]
async fn js_fs_large_binary_payloads_round_trip_under_memory_cap() {
    // Seeds data host-side so this test exercises the binary host-call ABI
    // directly: JS reads 8 MiB, verifies spot bytes, and writes it back.
    const SIZE: usize = 8 * 1024 * 1024;

    let vfs = Arc::new(InMemoryVfs::default());
    let input = (0..SIZE)
        .map(|index| (index.wrapping_mul(31).wrapping_add(7)) as u8)
        .collect::<Vec<_>>();
    write_vfs_file(vfs.as_ref(), "/big.bin", &input);

    let sandbox_vfs: Arc<dyn Vfs> = vfs.clone();
    let sandbox = Sandbox::builder().vfs_arc(sandbox_vfs).build();
    let spot_index = 1_234_567;
    let script = format!(
        r#"
const fs = require('fs')
const data = fs.readFileSync('/big.bin')
console.log(data.length, data[0], data[{spot_index}], data[data.length - 1])
fs.writeFileSync('/copy.bin', data)
fs.writeFileSync('/small', 'abc')
const fd = fs.openSync('/small', 'r')
const small = Buffer.alloc(16)
const n = fs.readSync(fd, small, 0, 20 * 1024 * 1024, 0)
fs.closeSync(fd)
console.log(n, small.toString('utf8').slice(0, n))
"#
    );

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(&script)))
        .await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        format!(
            "{SIZE} {} {} {}\n3 abc\n",
            input[0],
            input[spot_index],
            input[SIZE - 1]
        )
    );
    assert_eq!(read_vfs_file(vfs.as_ref(), "/copy.bin"), input);
    assert!(result.metrics.peak_wasm_memory_bytes.unwrap_or_default() <= 64 * 1024 * 1024);
}

#[tokio::test]
async fn js_fs_write_string_position_overload_matches_node() {
    // Node string overload is writeSync(fd, string[, position[, encoding]]).
    let sandbox = Sandbox::builder().build();
    let script = r#"
const fs = require('fs')
fs.writeFileSync('/out', 'hello world')
const fd = fs.openSync('/out', 'r+')
const n = fs.writeSync(fd, 'XY', 0)
fs.closeSync(fd)
console.log(n, fs.readFileSync('/out', 'utf8'))
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "2 XYllo world\n");
}

#[tokio::test]
async fn js_console_formatting_matches_node_for_supported_shapes() {
    // Fixtures are direct Node output for arrays/objects, util.format
    // substitutions, -0, and default object depth.
    let sandbox = Sandbox::builder().build();
    let script = r#"
console.log(['a', 'b'])
console.log({ s: 'x' })
console.log('%d %i %f %s %j %o %O %%', 3.4, 3.8, 3.25, 'x', { a: 1 }, { b: 'y' }, { c: 'z' }, 'extra')
console.log(-0)
console.log({ a: { b: { c: 1 } } })
const circular = {}
circular.self = circular
console.log(circular)
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0);
    assert_eq!(
        result.stdout,
        "[ 'a', 'b' ]\n{ s: 'x' }\n3.4 3 3.25 x {\"a\":1} { b: 'y' } { c: 'z' } % extra\n-0\n{ a: { b: { c: 1 } } }\n<ref *1> { self: [Circular *1] }\n"
    );
}

#[tokio::test]
async fn js_fs_readdir_with_file_types_returns_dirents() {
    // Node Dirents expose name plus isFile/isDirectory methods for this case.
    let sandbox = Sandbox::builder().build();
    let script = r#"
const fs = require('fs')
fs.mkdirSync('/dir')
fs.writeFileSync('/dir/file', 'x')
fs.mkdirSync('/dir/sub')
const entries = fs.readdirSync('/dir', { withFileTypes: true })
  .sort((a, b) => a.name.localeCompare(b.name))
for (const entry of entries) console.log(entry.name, entry.isFile(), entry.isDirectory())
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "file true false\nsub false true\n");
}

#[tokio::test]
async fn js_fs_errors_use_libuv_errno_values() {
    // Node v24.15.0 reports ENOTEMPTY as -66 through libuv, unlike Linux errno.
    let sandbox = Sandbox::builder().build();
    let script = r#"
const fs = require('fs')
fs.mkdirSync('/dir')
fs.writeFileSync('/dir/file', 'x')
try { fs.rmdirSync('/dir') } catch (err) { console.log(err.code, err.errno) }
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "ENOTEMPTY -66\n");
}

#[tokio::test]
async fn js_commonjs_does_not_expose_internal_host_globals() {
    // CommonJS setup should not reintroduce the raw ABI globals hidden by the
    // bootstrap glue.
    let sandbox = Sandbox::builder().build();
    let script = r#"
require('fs')
console.log([
  '__tinysandbox_host_call',
  '__tinysandbox_stdout',
  '__tinysandbox_stderr',
  '__tinysandbox_exit',
  '__tinysandbox_eval_module',
  '__tinysandboxConfig'
].map(name => typeof globalThis[name]).join(','))
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;

    assert_eq!(result.exit_code, 0);
    assert_eq!(
        result.stdout,
        "undefined,undefined,undefined,undefined,undefined,undefined\n"
    );
}

#[tokio::test]
async fn js_fs_errors_are_node_shaped_and_quota_errors_surface() {
    // JS catches errno-shaped errors from the VFS and sees the Node-style code
    // and message fields rather than a Rust/internal failure.
    let sandbox = Sandbox::builder()
        .vfs(InMemoryVfs::new(VfsQuota {
            max_bytes: 4,
            max_files: 8,
            max_file_size: 4,
        }))
        .build();
    let script = r#"
const fs = require('fs')
try { fs.readFileSync('/missing') } catch (err) { console.log(err.code, err.message) }
try { fs.writeFileSync('/too-big', 'abcdef') } catch (err) { console.log(err.code, err.message) }
console.log(fs.existsSync('/missing'))
"#;

    let result = sandbox
        .exec(&format!("js -e '{}'", shell_single_quote(script)))
        .await;
    assert_eq!(result.exit_code, 0);
    assert!(
        result
            .stdout
            .contains("ENOENT ENOENT: no such file or directory, open '/missing'")
    );
    assert!(
        result
            .stdout
            .contains("ENOSPC ENOSPC: no space left on device, open '/too-big'")
    );
    assert!(result.stdout.ends_with("false\n"));
}

#[tokio::test]
async fn js_commonjs_resolves_paths_and_sets_module_globals() {
    // These expectations mirror the same fixture tree under Node v24.15.0:
    // relative paths resolve from the requiring file, not process.cwd().
    let vfs = Arc::new(InMemoryVfs::default());
    seed_vfs(
        vfs.as_ref(),
        &["/app", "/app/sub", "/app/dir"],
        &[
            (
                "/app/main.js",
                r#"
const h = require('./helper.js')
console.log(h.fn())
console.log(require('./helper') === h)
console.log(require('./sub/child').value)
console.log(require('/app/dir'))
console.log(require('./data').name, require('./data.json').flag)
console.log(__filename)
console.log(__dirname)
console.log(require.main === module)
console.log(require('./sub/main-check'))
"#,
            ),
            (
                "/app/helper.js",
                "exports.fn = () => `help:${__dirname}:${__filename}`\n",
            ),
            (
                "/app/sub/child.js",
                "module.exports = { value: require('../helper').fn() }\n",
            ),
            (
                "/app/sub/main-check.js",
                "module.exports = require.main === module\n",
            ),
            ("/app/dir/index.js", "module.exports = 'indexed'\n"),
            ("/app/data.json", r#"{"name":"tinysandbox","flag":true}"#),
        ],
    );
    let sandbox_vfs: Arc<dyn Vfs> = vfs;
    let sandbox = Sandbox::builder()
        .vfs_arc(sandbox_vfs)
        .cwd("/elsewhere")
        .build();

    let result = sandbox.exec("js /app/main.js").await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "help:/app:/app/helper.js\ntrue\nhelp:/app:/app/helper.js\nindexed\ntinysandbox true\n/app/main.js\n/app\ntrue\nfalse\n"
    );
}

#[tokio::test]
async fn js_commonjs_trailing_slash_uses_directory_resolution_only() {
    // Node v24.15.0 resolves trailing slash specifiers through directory
    // index.js only: it chooses dir/index.js over dir.js and rejects x/ even
    // when x.js exists.
    let vfs = Arc::new(InMemoryVfs::default());
    seed_vfs(
        vfs.as_ref(),
        &["/app", "/app/dir"],
        &[
            (
                "/app/main.js",
                r#"
console.log(require('./dir/'))
try { require('./x/') } catch (err) {
  console.log(err.code)
  console.log(err.message === "Cannot find module './x/'\nRequire stack:\n- /app/main.js")
}
"#,
            ),
            ("/app/dir.js", "module.exports = 'file'\n"),
            ("/app/dir/index.js", "module.exports = 'index'\n"),
            ("/app/x.js", "module.exports = 'x-file'\n"),
        ],
    );
    let sandbox_vfs: Arc<dyn Vfs> = vfs;
    let sandbox = Sandbox::builder().vfs_arc(sandbox_vfs).build();

    let result = sandbox.exec("js /app/main.js").await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "index\nMODULE_NOT_FOUND\ntrue\n");
}

#[tokio::test]
async fn js_commonjs_bare_dot_and_dotdot_are_directory_specifiers() {
    // Node v24.15.0 treats "." and ".." as relative directory requests:
    // require('.') loads the requiring directory's index.js, and require('..')
    // from a child loads the parent index.js.
    let vfs = Arc::new(InMemoryVfs::default());
    seed_vfs(
        vfs.as_ref(),
        &["/app", "/app/sub"],
        &[
            (
                "/app/main.js",
                r#"
console.log(require('.'))
console.log(require('./sub/child'))
"#,
            ),
            ("/app/index.js", "module.exports = 'app-index'\n"),
            ("/app.js", "module.exports = 'app-file'\n"),
            ("/app/sub/child.js", "module.exports = require('..')\n"),
        ],
    );
    let sandbox_vfs: Arc<dyn Vfs> = vfs;
    let sandbox = Sandbox::builder().vfs_arc(sandbox_vfs).build();

    let result = sandbox.exec("js /app/main.js").await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "app-index\napp-index\n");
}

#[tokio::test]
async fn js_commonjs_caches_modules_and_returns_partial_cycle_exports() {
    // Node inserts a module into the cache before executing it, so side effects
    // happen once and a cycle observes the other module's current exports.
    let vfs = Arc::new(InMemoryVfs::default());
    seed_vfs(
        vfs.as_ref(),
        &["/app"],
        &[
            (
                "/app/main.js",
                r#"
const first = require('./counter')
const second = require('./counter')
console.log('same', first === second)
const a = require('./a')
const b = require('./b')
console.log('main', a.done, b.done)
"#,
            ),
            (
                "/app/counter.js",
                "console.log('counter loaded')\nmodule.exports = { marker: {} }\n",
            ),
            (
                "/app/a.js",
                r#"
exports.done = false
const b = require('./b')
console.log('in a, b.done =', b.done)
exports.done = true
"#,
            ),
            (
                "/app/b.js",
                r#"
exports.done = false
const a = require('./a')
console.log('in b, a.done =', a.done)
exports.done = true
"#,
            ),
        ],
    );
    let sandbox_vfs: Arc<dyn Vfs> = vfs;
    let sandbox = Sandbox::builder().vfs_arc(sandbox_vfs).build();

    let result = sandbox.exec("js /app/main.js").await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "counter loaded\nsame true\nin b, a.done = false\nin a, b.done = true\nmain true true\n"
    );
}

#[tokio::test]
async fn js_commonjs_reports_module_not_found_and_bare_specifiers_loudly() {
    // The relative MODULE_NOT_FOUND shape follows Node's code/message/stack;
    // bare packages add tinysandbox's explicit no-node_modules reason.
    let vfs = Arc::new(InMemoryVfs::default());
    seed_vfs(
        vfs.as_ref(),
        &["/app"],
        &[(
            "/app/main.js",
            r#"
try { require('./missing') } catch (err) {
  console.log(err.code)
  console.log(err.message === "Cannot find module './missing'\nRequire stack:\n- /app/main.js")
  console.log(err.requireStack.join('|'))
}
try { require('left-pad') } catch (err) {
  console.log(err.code)
  console.log(err.message.includes('no node_modules in tinysandbox'))
}
"#,
        )],
    );
    let sandbox_vfs: Arc<dyn Vfs> = vfs;
    let sandbox = Sandbox::builder().vfs_arc(sandbox_vfs).build();

    let result = sandbox.exec("js /app/main.js").await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "MODULE_NOT_FOUND\ntrue\n/app/main.js\nMODULE_NOT_FOUND\ntrue\n"
    );
}

#[tokio::test]
async fn js_commonjs_json_and_exports_alias_match_node_semantics() {
    // JSON modules export the parsed value, while rebinding `exports` alone
    // does not replace `module.exports`.
    let vfs = Arc::new(InMemoryVfs::default());
    seed_vfs(
        vfs.as_ref(),
        &["/app"],
        &[
            (
                "/app/main.js",
                r#"
console.log(JSON.stringify(require('./alias')))
console.log(require('./valid.json').nested.value)
try { require('./bad.json') } catch (err) {
  console.log(err.name)
  console.log(err.message.includes('/app/bad.json'))
  console.log(err.code === undefined)
}
"#,
            ),
            (
                "/app/alias.js",
                r#"
exports.a = 1
exports = { a: 2 }
module.exports.b = 3
module.exports = { c: 4 }
exports.d = 5
"#,
            ),
            ("/app/valid.json", r#"{"nested":{"value":7}}"#),
            ("/app/bad.json", "{ nope"),
        ],
    );
    let sandbox_vfs: Arc<dyn Vfs> = vfs;
    let sandbox = Sandbox::builder().vfs_arc(sandbox_vfs).build();

    let result = sandbox.exec("js /app/main.js").await;

    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "{\"c\":4}\n7\nSyntaxError\ntrue\ntrue\n");
}

#[tokio::test]
async fn js_commonjs_required_module_errors_keep_required_filename_in_stack() {
    // Required modules are evaled with their resolved filename so uncaught
    // stacks identify the throwing file, matching Node's debugging surface.
    let vfs = Arc::new(InMemoryVfs::default());
    seed_vfs(
        vfs.as_ref(),
        &["/app"],
        &[
            ("/app/main.js", "require('./helper')\n"),
            (
                "/app/helper.js",
                r#"
function boom() {
  throw new Error('helper boom')
}
boom()
"#,
            ),
        ],
    );
    let sandbox_vfs: Arc<dyn Vfs> = vfs;
    let sandbox = Sandbox::builder().vfs_arc(sandbox_vfs).build();

    let result = sandbox.exec("js /app/main.js").await;

    assert_eq!(result.exit_code, 1);
    assert!(result.stderr.starts_with("Error: helper boom\n"));
    assert!(
        result.stderr.contains("/app/helper.js"),
        "{}",
        result.stderr
    );
    assert!(!result.stderr.contains("wasm trap"));
}

#[tokio::test]
async fn js_commonjs_deep_require_chains_are_bounded_cleanly() {
    // A 200-module chain runs under the cap; a longer chain throws a catchable
    // JS error instead of reaching a wasm stack trap.
    let vfs = Arc::new(InMemoryVfs::default());
    seed_vfs(vfs.as_ref(), &["/chain", "/cap"], &[]);
    for index in 0..=200 {
        let source = if index == 200 {
            "module.exports = 200\n".to_owned()
        } else {
            format!("module.exports = require('./m{}')\n", index + 1)
        };
        write_vfs_file(
            vfs.as_ref(),
            &format!("/chain/m{index}.js"),
            source.as_bytes(),
        );
    }
    for index in 0..=260 {
        let source = if index == 260 {
            "module.exports = 260\n".to_owned()
        } else {
            format!("module.exports = require('./m{}')\n", index + 1)
        };
        write_vfs_file(
            vfs.as_ref(),
            &format!("/cap/m{index}.js"),
            source.as_bytes(),
        );
    }
    write_vfs_file(
        vfs.as_ref(),
        "/chain/main.js",
        b"console.log(require('./m0'))\n",
    );
    write_vfs_file(
        vfs.as_ref(),
        "/cap/main.js",
        b"try { require('./m0'); console.log('unexpected') } catch (err) { console.log(err.code); console.log(err.message.includes('256')) }\n",
    );
    let sandbox_vfs: Arc<dyn Vfs> = vfs;
    let sandbox = Sandbox::builder().vfs_arc(sandbox_vfs).build();

    let successful = sandbox.exec("js /chain/main.js").await;
    assert_eq!(successful.exit_code, 0, "stderr: {}", successful.stderr);
    assert_eq!(successful.stdout, "200\n");

    let capped = sandbox.exec("js /cap/main.js").await;
    assert_eq!(capped.exit_code, 0, "stderr: {}", capped.stderr);
    assert_eq!(capped.stdout, "ERR_REQUIRE_DEPTH\ntrue\n");
    assert!(!capped.stderr.contains("wasm trap"));
}

#[tokio::test]
async fn js_pipeline_and_redirects_use_command_stdio() {
    // The JS phase does not expose stdin to scripts yet, but command stdout is
    // still ordinary pipeline/redirect data handled by the shell executor.
    let sandbox = Sandbox::builder().build();
    assert_eq!(
        sandbox
            .exec("js -e 'console.log(\"alpha\"); console.log(\"beta\")' | grep beta > /out")
            .await
            .exit_code,
        0
    );
    assert_eq!(sandbox.exec("cat /out").await.stdout, "beta\n");
}

#[tokio::test]
async fn js_cpu_and_memory_limits_fail_cleanly() {
    // Epoch interruption should stop tight loops promptly with the same 124
    // timeout status used by the sandbox wall-clock guard.
    let sandbox = Sandbox::builder()
        .limits(Limits {
            wall_time: Duration::from_millis(30),
            ..Limits::default()
        })
        .build();
    let start = Instant::now();
    let result = sandbox.exec("js -e 'while (true) {}'").await;
    assert_eq!(result.exit_code, 124);
    assert!(start.elapsed() < Duration::from_secs(2));

    let oom = Sandbox::builder()
        .limits(Limits {
            wasm_memory_bytes: 4 * 1024 * 1024,
            ..Limits::default()
        })
        .build()
        .exec("js -e 'const chunks = []; while (true) chunks.push(new ArrayBuffer(1024 * 1024))'")
        .await;
    assert_ne!(oom.exit_code, 0);
    assert_ne!(oom.exit_code, 124);
    assert!(oom.stderr.contains("wasm memory limit exceeded"));
    assert!(oom.metrics.peak_wasm_memory_bytes.unwrap_or_default() <= 4 * 1024 * 1024);
}

fn shell_single_quote(input: &str) -> String {
    input.replace('\'', "'\\''")
}

fn seed_vfs(vfs: &dyn Vfs, dirs: &[&str], files: &[(&str, &str)]) {
    for dir in dirs {
        vfs.mkdir(dir).expect("create fixture directory");
    }
    for (path, data) in files {
        write_vfs_file(vfs, path, data.as_bytes());
    }
}

fn write_vfs_file(vfs: &dyn Vfs, path: &str, data: &[u8]) {
    let handle = vfs
        .open(path, OpenMode::write_only().create().truncate())
        .expect("open seeded file for writing");
    let mut written = 0;
    while written < data.len() {
        let n = vfs
            .write_at(
                handle,
                u64::try_from(written).expect("offset fits in u64"),
                &data[written..],
            )
            .expect("write seeded file");
        assert!(n > 0, "VFS write made no progress");
        written += n;
    }
    vfs.close(handle).expect("close seeded file");
}

fn read_vfs_file(vfs: &dyn Vfs, path: &str) -> Vec<u8> {
    let metadata = vfs.stat(path).expect("stat copied file");
    let handle = vfs
        .open(path, OpenMode::read_only())
        .expect("open copied file for reading");
    let mut out = vec![0; usize::try_from(metadata.len).expect("file length fits in usize")];
    let mut offset = 0;
    while offset < out.len() {
        let n = vfs
            .read_at(
                handle,
                u64::try_from(offset).expect("offset fits in u64"),
                &mut out[offset..],
            )
            .expect("read copied file");
        assert!(n > 0, "VFS read made no progress");
        offset += n;
    }
    vfs.close(handle).expect("close copied file");
    out
}
