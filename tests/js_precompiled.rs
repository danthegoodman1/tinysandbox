#![cfg(feature = "js")]

use tinysandbox::js::RuntimeSource;
use tinysandbox::sandbox::Sandbox;

#[tokio::test]
async fn precompiled_runtime_replaces_the_cranelift_compile() {
    // One test per binary: installing the runtime is a process-wide, one-time
    // action, so ordering against other tests would be unreliable.
    let artifact = tinysandbox::js::precompile().expect("precompile quickjs");
    assert!(!artifact.is_empty());

    // SAFETY: these bytes were just produced by our trusted compiler.
    unsafe { tinysandbox::js::use_precompiled(&artifact) }.expect("install precompiled runtime");
    assert_eq!(
        tinysandbox::js::runtime_source().expect("runtime source"),
        RuntimeSource::Precompiled
    );

    let sandbox = Sandbox::builder()
        .js_global("tools.answer", |_args| async { Ok(serde_json::json!(42)) })
        .build();
    let result = sandbox.exec("js -e 'console.log(tools.answer())'").await;
    assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
    assert_eq!(result.stdout, "42\n");

    // SAFETY: the artifact remains unmodified trusted compiler output.
    let second = unsafe { tinysandbox::js::use_precompiled(&artifact) };
    assert!(
        second
            .expect_err("second install must fail")
            .to_string()
            .contains("already initialized")
    );

    // The installed runtime keeps serving later commands.
    let again = sandbox.exec("js -e 'console.log(1 + 1)'").await;
    assert_eq!(again.stdout, "2\n");
}
