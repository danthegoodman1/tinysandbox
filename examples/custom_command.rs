//! Registering host commands that agents can call like any builtin.
//!
//! Run with: cargo run --example custom_command

use tinysandbox::sandbox::{CommandContext, CommandResult, Sandbox};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() {
    let sandbox = Sandbox::builder()
        // A command is an async fn from CommandContext to an exit code. This
        // one reads stdin, uppercases it, and writes the result to a VFS path
        // given as an argument (or stdout when no argument is given).
        .command("shout", |mut ctx: CommandContext| async move {
            let mut input = String::new();
            if ctx.stdin.read_to_string(&mut input).await.is_err() {
                return CommandResult::failure();
            }
            let output = input.to_uppercase();
            match ctx.args.first() {
                Some(path) => {
                    if let Err(err) = ctx.fs.write_file(path, output.as_bytes(), false).await {
                        let _ = ctx
                            .stderr
                            .write_all(format!("shout: {path}: {err:?}\n").as_bytes())
                            .await;
                        return CommandResult::failure();
                    }
                }
                None => {
                    let _ = ctx.stdout.write_all(output.as_bytes()).await;
                }
            }
            CommandResult::success()
        })
        .build();

    // Custom commands appear in /bin and resolve via which, so the
    // environment stays coherent when an agent probes it.
    let result = sandbox.exec("which shout && ls /bin | grep shout").await;
    print!("{}", result.stdout);

    // They compose with builtins in pipelines and redirects.
    let result = sandbox.exec("echo make some noise | shout").await;
    assert_eq!(result.stdout, "MAKE SOME NOISE\n");
    print!("{}", result.stdout);

    sandbox.exec("echo quiet words | shout /loud.txt").await;
    let result = sandbox.exec("cat /loud.txt").await;
    assert_eq!(result.stdout, "QUIET WORDS\n");
    print!("{}", result.stdout);
}
