//! Changing the host global surface between commands.
//!
//! Every `js` command snapshots the sandbox's global registry when it starts,
//! so the surface can change between turns without rebuilding the sandbox and
//! without a running script ever seeing globals appear or vanish.
//!
//! Run with: cargo run --example js_dynamic_globals

use serde_json::{Value, json};
use tinysandbox::sandbox::{HostError, JsGlobals, Sandbox};

/// A base capability every turn keeps, written as a plain function so the same
/// handler can be re-granted after a revoking swap.
async fn whoami(_args: Value) -> Result<Value, HostError> {
    Ok(json!("agent-1"))
}

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder().js_global("whoami", whoami).build();
    println!("base:    {:?}", sandbox.js_global_names());

    // Turn one adds tools and keeps everything already bound.
    sandbox
        .extend_js_globals(
            JsGlobals::new()
                .with("tools.search", |args| async move {
                    Ok(json!({ "hits": [format!("hit for {}", args["q"].as_str().unwrap_or(""))] }))
                })
                .with("tools.read_doc", |_args| async { Ok(json!("doc body")) }),
        )
        .expect("grant turn-one tools");
    println!("turn 1:  {:?}", sandbox.js_global_names());
    let result = sandbox
        .exec(r#"js -e 'console.log(whoami(), tools.search({ q: "vfs" }).hits[0])'"#)
        .await;
    print!("call:    {}", result.stdout);

    // Turn two revokes turn one. `replace` swaps the whole surface, so the base
    // capability is re-granted alongside the new tool.
    sandbox
        .replace_js_globals(
            JsGlobals::new()
                .with("whoami", whoami)
                .with("tools.write_note", |args| async move {
                    Ok(json!({ "written": args["text"].as_str().unwrap_or("").len() }))
                }),
        )
        .expect("grant turn-two tools");
    println!("turn 2:  {:?}", sandbox.js_global_names());
    let revoked = sandbox
        .exec("js -e 'console.log(typeof tools.search)'")
        .await;
    print!("revoked: {}", revoked.stdout);

    // Single names can be added and dropped without touching the rest.
    sandbox
        .set_js_global("tools.trace", |_args| async { Ok(json!("traced")) })
        .expect("add tools.trace");
    println!("added:   {:?}", sandbox.js_global_names());
    println!("removed: {}", sandbox.remove_js_global("tools.trace"));
    println!("again:   {}", sandbox.remove_js_global("tools.trace"));

    // The live API reports what the builder would panic on, and a rejected
    // change leaves the surface exactly as it was.
    let reserved = sandbox
        .set_js_global("console", |_args| async { Ok(json!(null)) })
        .expect_err("reserved name");
    println!("refused: {reserved}");
    let conflict = sandbox
        .extend_js_globals(JsGlobals::new().with("tools", |_args| async { Ok(json!(null)) }))
        .expect_err("namespace in use");
    println!("refused: {conflict}");
    println!("bound:   {:?}", sandbox.js_global_names());
}
