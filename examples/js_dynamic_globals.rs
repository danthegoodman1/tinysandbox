//! Changing the host global surface between commands.
//!
//! Every `js` command snapshots the sandbox's global registry when it starts,
//! so the surface can change between turns without rebuilding the sandbox and
//! without a running script ever seeing globals appear or vanish.
//!
//! Run with: cargo run --example js_dynamic_globals

use serde_json::json;
use tinysandbox::sandbox::{JsGlobals, Sandbox};

const LIST: &str = r#"js -e 'console.log(typeof tools === "undefined" ? "(none)" : Object.keys(tools).join(","))'"#;

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder().build();

    // Turn one grants a read-only surface. One swap, validated as a whole: a
    // rejected set leaves the previous turn's globals in place.
    sandbox
        .replace_js_globals(
            JsGlobals::new()
                .with("tools.search", |args| async move {
                    Ok(json!({ "hits": [format!("hit for {}", args["q"].as_str().unwrap_or(""))] }))
                })
                .with("tools.read_doc", |_args| async { Ok(json!("doc body")) }),
        )
        .expect("grant turn-one tools");
    println!(
        "turn 1 tools: {}",
        sandbox.exec(LIST).await.stdout.trim_end()
    );
    let result = sandbox
        .exec(r#"js -e 'console.log(tools.search({ q: "vfs" }).hits[0])'"#)
        .await;
    print!("turn 1 call: {}", result.stdout);

    // Turn two revokes those and grants a writer instead.
    sandbox
        .replace_js_globals(
            JsGlobals::new().with("tools.write_note", |args| async move {
                Ok(json!({ "written": args["text"].as_str().unwrap_or("").len() }))
            }),
        )
        .expect("grant turn-two tools");
    println!(
        "turn 2 tools: {}",
        sandbox.exec(LIST).await.stdout.trim_end()
    );
    let revoked = sandbox
        .exec("js -e 'console.log(typeof tools.search)'")
        .await;
    print!("turn 2 sees search: {}", revoked.stdout);

    // Single names can be added and dropped without touching the rest.
    sandbox
        .set_js_global("whoami", |_args| async { Ok(json!("agent-1")) })
        .expect("add whoami");
    let added = sandbox.exec("js -e 'console.log(whoami())'").await;
    print!("added whoami: {}", added.stdout);
    println!("removed whoami: {}", sandbox.remove_js_global("whoami"));
    println!("removed again: {}", sandbox.remove_js_global("whoami"));
    println!("bound now: {:?}", sandbox.js_global_names());

    // The live API reports what the builder would panic on.
    let rejected = sandbox
        .set_js_global("console", |_args| async { Ok(json!(null)) })
        .expect_err("reserved name");
    println!("rejected: {rejected}");
    let conflict = sandbox
        .set_js_global("tools", |_args| async { Ok(json!(null)) })
        .expect_err("namespace in use");
    println!("rejected: {conflict}");
}
