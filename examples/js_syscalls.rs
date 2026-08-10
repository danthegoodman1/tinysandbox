//! JavaScript syscalls, prelude wrappers, and embedder-backed fetch.
//!
//! Run with: cargo run --example js_syscalls

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tinysandbox::sandbox::{FetchResponse, Sandbox, SyscallError};

#[tokio::main]
async fn main() {
    let store = Arc::new(Mutex::new(HashMap::<String, Value>::new()));
    let get_store = Arc::clone(&store);
    let put_store = Arc::clone(&store);

    let sandbox = Sandbox::builder()
        .syscall("kv_get", move |args| {
            let store = Arc::clone(&get_store);
            async move {
                let key = string_arg(&args, "key")?;
                let value = store.lock().expect("kv store lock").get(key).cloned();
                Ok(json!({ "value": value.unwrap_or(Value::Null) }))
            }
        })
        .syscall("kv_put", move |args| {
            let store = Arc::clone(&put_store);
            async move {
                let key = string_arg(&args, "key")?.to_owned();
                let value = args.get("value").cloned().unwrap_or(Value::Null);
                store.lock().expect("kv store lock").insert(key, value);
                Ok(json!({ "ok": true }))
            }
        })
        .js_prelude("globalThis.kvGet = key => sandbox.kv_get({ key }).value")
        .fetch(|request| async move {
            if request.url == "https://example.test/config" {
                Ok(FetchResponse {
                    status: 200,
                    headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
                    body: b"feature=on".to_vec(),
                })
            } else {
                Err(
                    SyscallError::new(format!("no canned response for {}", request.url))
                        .with_code("ENOENT"),
                )
            }
        })
        .build();

    let script = r#"
sandbox.kv_put({ key: 'answer', value: 42 })
console.log(`answer=${kvGet('answer')}`)

try {
  sandbox.kv_get({})
} catch (err) {
  console.log(`${err.code}:${err.message}`)
}

(async () => {
  const response = await fetch('https://example.test/config')
  console.log(`${response.status}:${await response.text()}`)
})()
"#;
    sandbox
        .fs()
        .write_file("/workspace/main.js", script.as_bytes(), false)
        .await
        .expect("write example script");

    let result = sandbox.exec("js /workspace/main.js").await;
    print!("{}", result.stdout);
    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(
        result.stdout,
        "answer=42\nE_KEY:key is required\n200:feature=on\n"
    );
}

fn string_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, SyscallError> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| SyscallError::new(format!("{name} is required")).with_code("E_KEY"))
}
