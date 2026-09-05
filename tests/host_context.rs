//! Public cooperative cancellation for trusted callbacks and custom commands.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tinysandbox::sandbox::{CommandResult, HostContext, Limits, Sandbox};

#[tokio::test]
async fn dropping_exec_wakes_retained_custom_command_context() {
    let (sent, received) = tokio::sync::oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(sent)));
    let sandbox = Sandbox::builder()
        .limits(Limits {
            wall_time: Duration::from_secs(10),
            ..Limits::default()
        })
        .command("hold", move |ctx| {
            let sender = Arc::clone(&sender);
            async move {
                sender
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(ctx.fs.host_context())
                    .unwrap();
                std::future::pending::<CommandResult>().await
            }
        })
        .build();
    let mut execution = Box::pin(sandbox.exec("hold"));
    let context = tokio::select! {
        context = received => context.unwrap(),
        result = &mut execution => panic!("command finished before cancellation: {result:?}"),
    };
    assert!(!context.is_cancelled());
    assert!(context.deadline().unwrap() > Instant::now());
    assert!(context.remaining().unwrap() > Duration::from_secs(5));
    drop(execution);
    tokio::time::timeout(Duration::from_millis(500), context.cancelled())
        .await
        .unwrap();
    assert!(context.is_cancelled());
}

#[tokio::test]
async fn completed_exec_cancels_context_but_host_filesystem_is_unscoped() {
    let saved = Arc::new(Mutex::new(None));
    let observed = Arc::clone(&saved);
    let sandbox = Sandbox::builder()
        .command("capture", move |ctx| {
            *observed.lock().unwrap() = Some(ctx.fs.host_context());
            async { CommandResult::success() }
        })
        .build();
    let host = sandbox.fs().host_context();
    assert!(host.deadline().is_none());
    assert!(host.remaining().is_none());
    assert!(!host.is_cancelled());
    assert_eq!(sandbox.exec("capture").await.exit_code, 0);
    let context: HostContext = saved.lock().unwrap().take().unwrap();
    assert!(context.is_cancelled());
    tokio::time::timeout(Duration::from_millis(100), context.cancelled())
        .await
        .unwrap();
    assert!(!host.is_cancelled());
}

#[cfg(feature = "js")]
#[tokio::test]
async fn contextual_global_registration_and_fetch_preserve_legacy_callbacks() {
    use serde_json::json;
    use tinysandbox::sandbox::{FetchResponse, JsGlobals};
    let sandbox = Sandbox::builder()
        .js_global("legacy", |_| async { Ok(json!(1)) })
        .js_global_with_context("scoped", |value, context| async move {
            assert!(context.deadline().unwrap() > Instant::now());
            assert!(!context.is_cancelled());
            assert!(context.remaining().unwrap() < Limits::default().wall_time);
            Ok(value)
        })
        .fetch_with_context(|request, context| async move {
            assert_eq!(request.url, "https://example.test/");
            assert!(context.deadline().is_some());
            assert!(!context.is_cancelled());
            Ok(FetchResponse {
                status: 200,
                headers: Vec::new(),
                body: b"fetch".to_vec(),
            })
        })
        .build();
    sandbox
        .set_js_global_with_context("dynamic", |_, context| async move {
            assert!(!context.is_cancelled());
            Ok(json!(3))
        })
        .unwrap();
    sandbox
        .extend_js_globals(
            JsGlobals::new().with_context("grouped", |_, context| async move {
                assert!(context.remaining().is_some());
                Ok(json!(4))
            }),
        )
        .unwrap();
    let result = sandbox.exec("js -e 'console.log(legacy(), scoped(2), dynamic(), grouped()); fetch(\"https://example.test/\").then(r => r.text()).then(console.log)'").await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "1 2 3 4\nfetch\n");
}

#[cfg(feature = "js")]
#[tokio::test]
async fn host_callback_timeout_is_visible_through_its_context() {
    use serde_json::Value;
    tinysandbox::js::runtime_source().unwrap();
    let saved = Arc::new(Mutex::new(None));
    let observed = Arc::clone(&saved);
    let sandbox = Sandbox::builder()
        .limits(Limits {
            wall_time: Duration::from_millis(500),
            ..Limits::default()
        })
        .js_global_with_context("hang", move |_, context| {
            *observed.lock().unwrap() = Some(context);
            std::future::pending::<Result<Value, tinysandbox::sandbox::HostError>>()
        })
        .build();
    let result = sandbox
        .exec("js -e 'try { hang() } catch (error) { console.log(error.message) }'")
        .await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "global 'hang' timed out\n");
    let context: HostContext = saved.lock().unwrap().take().unwrap();
    assert!(context.is_cancelled());
    tokio::time::timeout(Duration::from_millis(100), context.cancelled())
        .await
        .unwrap();
}

#[cfg(feature = "js")]
#[tokio::test]
async fn dropping_exec_wakes_a_running_global_context_before_its_deadline() {
    use serde_json::Value;
    tinysandbox::js::runtime_source().unwrap();
    let (sent, received) = tokio::sync::oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(sent)));
    let sandbox = Sandbox::builder()
        .limits(Limits {
            wall_time: Duration::from_secs(10),
            ..Limits::default()
        })
        .js_global_with_context("hold", move |_, context| {
            sender
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(context)
                .unwrap();
            std::future::pending::<Result<Value, tinysandbox::sandbox::HostError>>()
        })
        .build();
    let mut execution = Box::pin(sandbox.exec("js -e 'hold()'"));
    let context = tokio::select! {
        context = received => context.unwrap(),
        result = &mut execution => panic!("global finished before cancellation: {result:?}"),
    };
    assert!(!context.is_cancelled());
    drop(execution);
    tokio::time::timeout(Duration::from_millis(500), context.cancelled())
        .await
        .unwrap();
    assert!(context.is_cancelled());
    assert!(context.remaining().unwrap() > Duration::from_secs(5));
}

#[cfg(feature = "js")]
#[tokio::test]
async fn settled_callback_contexts_cancel_without_ending_the_execution() {
    use serde_json::{Value, json};
    use tinysandbox::sandbox::HostError;
    let saved = Arc::new(Mutex::new(Vec::<HostContext>::new()));
    let observed = Arc::clone(&saved);
    let completed = Arc::clone(&saved);
    let sandbox = Sandbox::builder()
        .js_global_with_context("complete", move |fail, context| {
            observed.lock().unwrap().push(context);
            async move {
                if fail == Value::Bool(true) {
                    Err(HostError::new("expected failure"))
                } else {
                    Ok(Value::Null)
                }
            }
        })
        .js_global_with_context("inspect", move |_, current| {
            let contexts = completed.lock().unwrap().clone();
            async move {
                assert!(!current.is_cancelled());
                assert!(current.remaining().unwrap() > Duration::from_secs(1));
                for context in &contexts {
                    assert!(
                        context.is_cancelled(),
                        "settled callback retained a live context"
                    );
                    assert!(context.remaining().unwrap() > Duration::from_secs(1));
                    tokio::time::timeout(Duration::from_millis(100), context.cancelled())
                        .await
                        .unwrap();
                }
                Ok(json!(contexts.len()))
            }
        })
        .build();
    let result = sandbox
        .exec("js -e 'complete(false); console.log(inspect()); try { complete(true) } catch (_) {} console.log(inspect())'")
        .await;
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout, "1\n2\n");
}
