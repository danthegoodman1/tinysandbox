//! Native command implementations.
//!
//! `grep` and `sed s///` deliberately use Rust's regular-expression dialect
//! instead of GNU BRE/ERE syntax so matching remains linear-time. Unsupported
//! GNU regex syntax therefore fails at compile time instead of being
//! interpreted differently.
//!
//! Line-oriented streaming commands cap any single buffered line at 1 MiB and
//! report `line too long`; plain `cat` and `wc` remain byte-streaming commands.
//!
//! Other known GNU deviations: `sed` reports all input read failures with
//! exit code 2 (GNU distinguishes mid-stream I/O errors with 4), and error
//! messages omit GNU's `Try '<tool> --help'` second line. Trailing slashes on
//! file paths (`ls file///`) are accepted because VFS path normalization
//! strips them, where POSIX would fail with ENOTDIR.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use regex::{Captures, Regex, RegexBuilder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};

use super::jq::{self, JqError};
use crate::sandbox::command::{
    BoxAsyncRead, BoxAsyncWrite, Command, CommandContext, CommandFuture, CommandResult,
};
use crate::sandbox::fs::{Fs, STREAM_CHUNK_BYTES, errno_message, join_path};
use crate::vfs::{DirEntry, Errno, FileType, Metadata, OpenMode, VfsError};

const MAX_STREAM_LINE_BYTES: usize = 1024 * 1024;
const LINE_TOO_LONG: &str = "line too long";
const MAX_JQ_JSON_NESTING: usize = 1024;
// Independent from JS admission: a native evaluator that outlives an exec
// retains its slot until it actually exits. Collect bounded stdin before
// admission so a queued downstream jq can drain an admitted producer's pipe.
const MAX_JQ_WORKERS: usize = 16;
static JQ_WORKERS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_JQ_WORKERS)));

pub(crate) fn register(commands: &mut BTreeMap<String, Arc<dyn Command>>) {
    insert(commands, "cat", cat);
    insert(commands, "cp", cp);
    insert(commands, "echo", echo);
    insert(commands, "false", false_cmd);
    insert(commands, "grep", grep);
    insert(commands, "head", head);
    insert(commands, "jq", jq_cmd);
    insert(commands, "ls", ls);
    insert(commands, "mkdir", mkdir);
    insert(commands, "mv", mv);
    insert(commands, "pwd", pwd);
    insert(commands, "rm", rm);
    insert(commands, "sed", sed);
    insert(commands, "sort", sort);
    insert(commands, "stat", stat);
    insert(commands, "tail", tail);
    insert(commands, "touch", touch);
    insert(commands, "true", true_cmd);
    insert(commands, "uniq", uniq);
    insert(commands, "wc", wc);
    insert(commands, "which", which);
}

fn insert<F>(commands: &mut BTreeMap<String, Arc<dyn Command>>, name: &'static str, f: F)
where
    F: Fn(CommandContext) -> CommandFuture + Send + Sync + 'static,
{
    commands.insert(name.to_owned(), Arc::new(f));
}

fn boxed(fut: impl Future<Output = CommandResult> + Send + 'static) -> CommandFuture {
    Box::pin(fut)
}

#[derive(Default)]
struct CatFlags {
    number: bool,
    number_nonblank: bool,
    squeeze: bool,
    show_ends: bool,
    show_tabs: bool,
}

impl CatFlags {
    fn plain(&self) -> bool {
        !(self.number || self.number_nonblank || self.squeeze || self.show_ends || self.show_tabs)
    }
}

// Line number and blank-run state persist across input files, matching GNU cat
// (numbering continues into the next file; -s squeezes across file boundaries).
#[derive(Default)]
struct CatState {
    line: u64,
    prev_blank: bool,
}

#[derive(Debug, Clone)]
struct JqOptions {
    filter: String,
    files: Vec<String>,
    raw_output: bool,
    join_output: bool,
    compact_output: bool,
    exit_status: bool,
    null_input: bool,
    slurp: bool,
    sort_keys: bool,
    indent: String,
    vars: Vec<JqVariable>,
}

#[derive(Debug, Clone)]
struct JqVariable {
    name: String,
    value: jaq_json::Val,
}

#[derive(Debug)]
struct JqInputSource {
    path: String,
    data: Vec<u8>,
}

struct JqRunDone {
    exit_code: i32,
    stderr: Vec<u8>,
}

enum JqStreamMessage {
    Stdout(Vec<u8>),
    Done(JqRunDone),
}

struct JqCancelOnDrop {
    cancelled: Arc<AtomicBool>,
}

impl Drop for JqCancelOnDrop {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
enum JqInputError {
    Vfs { path: String, err: VfsError },
    Read { path: String, err: io::Error },
    JsonDepth { path: String, err: JqJsonDepthError },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JqJsonDepthError {
    max: usize,
}

impl std::fmt::Display for JqJsonDepthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON nesting exceeds maximum depth {}", self.max)
    }
}

fn cat_transform(data: &[u8], flags: &CatFlags, state: &mut CatState, out: &mut Vec<u8>) {
    let mut rest = data;
    while !rest.is_empty() {
        let (content, had_newline) = match rest.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                let line = &rest[..pos];
                rest = &rest[pos + 1..];
                (line, true)
            }
            None => {
                let line = rest;
                rest = &[];
                (line, false)
            }
        };
        let blank = content.is_empty() && had_newline;
        if flags.squeeze && blank && state.prev_blank {
            continue;
        }
        state.prev_blank = blank;
        // GNU: -b numbers only non-blank lines and overrides -n.
        let numbered = if flags.number_nonblank {
            !blank
        } else {
            flags.number
        };
        if numbered {
            state.line += 1;
            out.extend_from_slice(format!("{:>6}\t", state.line).as_bytes());
        }
        if flags.show_tabs {
            for &b in content {
                if b == b'\t' {
                    out.extend_from_slice(b"^I");
                } else {
                    out.push(b);
                }
            }
        } else {
            out.extend_from_slice(content);
        }
        if had_newline {
            // -E marks the newline position, so a final line without a
            // trailing newline gets no '$'.
            if flags.show_ends {
                out.push(b'$');
            }
            out.push(b'\n');
        }
    }
}

fn cat(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stdin,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        let mut flags = CatFlags::default();
        let mut paths = Vec::new();
        for arg in args {
            if arg.starts_with('-') && arg != "-" {
                for flag in arg.chars().skip(1) {
                    match flag {
                        'n' => flags.number = true,
                        'b' => flags.number_nonblank = true,
                        's' => flags.squeeze = true,
                        'E' => flags.show_ends = true,
                        'T' => flags.show_tabs = true,
                        _ => return unsupported("cat", &format!("-{flag}"), &mut stderr).await,
                    }
                }
            } else {
                paths.push(arg);
            }
        }
        if paths.is_empty() {
            paths.push("-".to_string());
        }

        let mut state = CatState::default();
        let mut exit = 0;
        for path in paths {
            if path == "-" {
                if let Err(err) = cat_stream(&mut stdin, &mut stdout, &flags, &mut state).await {
                    if report_stream_error(&mut stderr, "cat", &path, err).await {
                        exit = 1;
                        continue;
                    }
                    return CommandResult::failure();
                }
            } else {
                match fs.stream_reader(&path).await {
                    Ok(mut reader) => {
                        if let Err(err) =
                            cat_stream(&mut reader, &mut stdout, &flags, &mut state).await
                        {
                            if report_stream_error(&mut stderr, "cat", &path, err).await {
                                exit = 1;
                                continue;
                            }
                            return CommandResult::failure();
                        }
                    }
                    Err(err) => {
                        exit = 1;
                        write_vfs_error(&mut stderr, "cat", &path, err).await;
                    }
                }
            }
        }
        CommandResult::new(exit)
    })
}

async fn cat_stream(
    reader: &mut BoxAsyncRead,
    stdout: &mut BoxAsyncWrite,
    flags: &CatFlags,
    state: &mut CatState,
) -> io::Result<()> {
    let mut buf = vec![0; 64 * 1024];
    if flags.plain() {
        loop {
            tokio::task::yield_now().await;
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            stdout.write_all(&buf[..n]).await?;
        }
    }

    let mut pending = LineBuffer::default();
    while let Some(line) = read_line(reader, &mut pending).await? {
        let mut out = Vec::new();
        cat_transform(&line, flags, state, &mut out);
        stdout.write_all(&out).await?;
    }
    Ok(())
}

fn echo(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args, mut stdout, ..
        } = ctx;
        let mut newline = true;
        let mut escapes = false;
        let mut first = 0;
        while first < args.len() {
            let arg = args[first].as_str();
            if arg.len() <= 1 || !arg.starts_with('-') {
                break;
            }
            let options = &arg[1..];
            if !options.chars().all(|ch| matches!(ch, 'n' | 'e' | 'E')) {
                break;
            }
            for option in options.chars() {
                match option {
                    'n' => newline = false,
                    'e' => escapes = true,
                    'E' => escapes = false,
                    _ => unreachable!("validated echo option"),
                }
            }
            first += 1;
        }

        let mut out = args[first..].join(" ");
        let mut suppress_rest = false;
        if escapes {
            let (expanded, stop) = expand_echo_escapes(&out);
            out = expanded;
            suppress_rest = stop;
        }
        if newline && !suppress_rest {
            out.push('\n');
        }
        if stdout.write_all(out.as_bytes()).await.is_err() {
            return CommandResult::failure();
        }
        CommandResult::success()
    })
}

fn pwd(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            cwd,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        if let Some(flag) = unsupported_flag(&args) {
            return unsupported("pwd", flag, &mut stderr).await;
        }
        if stdout
            .write_all(format!("{cwd}\n").as_bytes())
            .await
            .is_err()
        {
            return CommandResult::failure();
        }
        CommandResult::success()
    })
}

fn true_cmd(_ctx: CommandContext) -> CommandFuture {
    boxed(async { CommandResult::success() })
}

fn false_cmd(_ctx: CommandContext) -> CommandFuture {
    boxed(async { CommandResult::new(1) })
}

fn which(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            commands,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        if args.is_empty() {
            let _ = stderr.write_all(b"which: missing operand\n").await;
            return CommandResult::new(1);
        }
        let mut exit = 0;
        for name in args {
            if commands.contains(&name) {
                let _ = stdout.write_all(format!("/bin/{name}\n").as_bytes()).await;
            } else {
                exit = 1;
            }
        }
        CommandResult::new(exit)
    })
}

fn jq_cmd(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            limits,
            mut stdin,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        let options = match parse_jq_args(args) {
            Ok(options) => options,
            Err(message) => {
                let _ = stderr.write_all(message.as_bytes()).await;
                return CommandResult::new(2);
            }
        };

        let inputs = if options.null_input {
            Vec::new()
        } else {
            match read_jq_inputs(&fs, &options.files, &mut stdin, limits.jq_input_bytes).await {
                Ok(inputs) => inputs,
                Err(err) => {
                    write_jq_input_error(&mut stderr, err).await;
                    return CommandResult::new(2);
                }
            }
        };

        let permit = Arc::clone(&JQ_WORKERS)
            .acquire_owned()
            .await
            .expect("jq admission semaphore remains open");
        if fs.checkpoint().await.is_err() {
            return CommandResult::new(124);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = JqCancelOnDrop {
            cancelled: Arc::clone(&cancelled),
        };
        if fs.checkpoint().await.is_err() {
            return CommandResult::new(124);
        }
        let deadline = fs
            .remaining_wall_time()
            .and_then(|remaining| Instant::now().checked_add(remaining));
        let (tx, mut rx) = mpsc::channel(4);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            run_jq_program(options, inputs, tx, deadline, cancelled);
        });

        while let Some(message) = rx.recv().await {
            match message {
                JqStreamMessage::Stdout(chunk) => {
                    if let Err(err) = stdout.write_all(&chunk).await {
                        return if is_broken_pipe(&err) {
                            CommandResult::failure()
                        } else {
                            let _ = stderr
                                .write_all(format!("jq: output error: {err}\n").as_bytes())
                                .await;
                            CommandResult::new(5)
                        };
                    }
                }
                JqStreamMessage::Done(done) => {
                    if !done.stderr.is_empty() {
                        let _ = stderr.write_all(&done.stderr).await;
                    }
                    return CommandResult::new(done.exit_code);
                }
            }
        }

        CommandResult::failure()
    })
}

fn ls(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        let mut all = false;
        let mut long = false;
        let mut paths = Vec::new();
        for arg in args {
            if arg.starts_with('-') && arg != "-" {
                for flag in arg.chars().skip(1) {
                    match flag {
                        'a' => all = true,
                        'l' => long = true,
                        _ => return unsupported("ls", &format!("-{flag}"), &mut stderr).await,
                    }
                }
            } else {
                paths.push(arg);
            }
        }
        if paths.is_empty() {
            paths.push(".".to_owned());
        }

        let multiple = paths.len() > 1;
        let mut exit = 0;
        for (index, path) in paths.iter().enumerate() {
            match fs.stat(path).await {
                Ok(metadata) if metadata.file_type == FileType::Directory => {
                    if multiple {
                        if index > 0 {
                            let _ = stdout.write_all(b"\n").await;
                        }
                        let _ = stdout.write_all(format!("{path}:\n").as_bytes()).await;
                    }
                    match list_dir(&fs, path, all).await {
                        Ok(entries) => {
                            for (name, metadata) in entries {
                                write_ls_entry(&mut stdout, &name, metadata, long).await;
                            }
                        }
                        Err(err) => {
                            exit = 1;
                            write_vfs_error(&mut stderr, "ls", path, err).await;
                        }
                    }
                }
                Ok(metadata) => {
                    let name = basename(path);
                    write_ls_entry(&mut stdout, &name, metadata, long).await;
                }
                Err(err) => {
                    exit = 2;
                    write_vfs_error(&mut stderr, "ls", path, err).await;
                }
            }
        }
        CommandResult::new(exit)
    })
}

fn mkdir(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stderr,
            ..
        } = ctx;
        let mut parents = false;
        let mut paths = Vec::new();
        for arg in args {
            match arg.as_str() {
                "-p" => parents = true,
                flag if flag.starts_with('-') => {
                    return unsupported("mkdir", flag, &mut stderr).await;
                }
                _ => paths.push(arg),
            }
        }
        if paths.is_empty() {
            let _ = stderr.write_all(b"mkdir: missing operand\n").await;
            return CommandResult::new(1);
        }
        let mut exit = 0;
        for path in paths {
            let result = if parents {
                mkdir_p(&fs, &path).await
            } else {
                fs.mkdir(&path).await
            };
            if let Err(err) = result {
                exit = 1;
                write_vfs_error(&mut stderr, "mkdir", &path, err).await;
            }
        }
        CommandResult::new(exit)
    })
}

fn touch(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stderr,
            ..
        } = ctx;
        if let Some(flag) = unsupported_flag(&args) {
            return unsupported("touch", flag, &mut stderr).await;
        }
        if args.is_empty() {
            let _ = stderr.write_all(b"touch: missing file operand\n").await;
            return CommandResult::new(1);
        }
        let mut exit = 0;
        for path in args {
            if let Err(err) = fs.touch(&path).await {
                exit = 1;
                write_vfs_error(&mut stderr, "touch", &path, err).await;
            }
        }
        CommandResult::new(exit)
    })
}

fn cp(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stderr,
            ..
        } = ctx;
        let (recursive, paths) = match parse_recursive_args("cp", args, &mut stderr).await {
            Ok(parsed) => parsed,
            Err(result) => return result,
        };
        if paths.len() < 2 {
            let _ = stderr.write_all(b"cp: missing file operand\n").await;
            return CommandResult::new(1);
        }
        let dest = paths.last().cloned().unwrap_or_default();
        let sources = &paths[..paths.len() - 1];
        if sources.len() > 1
            && !matches!(fs.stat(&dest).await, Ok(meta) if meta.file_type == FileType::Directory)
        {
            let _ = stderr
                .write_all(format!("cp: target '{dest}': Not a directory\n").as_bytes())
                .await;
            return CommandResult::new(1);
        }

        let mut exit = 0;
        for source in sources {
            let target = destination_for(&fs, source, &dest).await;
            if let Err(err) = copy_path(&fs, source, &target, recursive).await {
                exit = 1;
                write_vfs_error(&mut stderr, "cp", source, err).await;
            }
        }
        CommandResult::new(exit)
    })
}

fn mv(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stderr,
            ..
        } = ctx;
        if let Some(flag) = unsupported_flag(&args) {
            return unsupported("mv", flag, &mut stderr).await;
        }
        if args.len() < 2 {
            let _ = stderr.write_all(b"mv: missing file operand\n").await;
            return CommandResult::new(1);
        }
        let dest = args.last().cloned().unwrap_or_default();
        let sources = &args[..args.len() - 1];
        if sources.len() > 1
            && !matches!(fs.stat(&dest).await, Ok(meta) if meta.file_type == FileType::Directory)
        {
            let _ = stderr
                .write_all(format!("mv: target '{dest}': Not a directory\n").as_bytes())
                .await;
            return CommandResult::new(1);
        }
        let mut exit = 0;
        for source in sources {
            let target = destination_for(&fs, source, &dest).await;
            if let Err(err) = fs.rename(source, &target).await {
                exit = 1;
                write_vfs_error(&mut stderr, "mv", source, err).await;
            }
        }
        CommandResult::new(exit)
    })
}

fn rm(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stderr,
            ..
        } = ctx;
        let mut recursive = false;
        let mut force = false;
        let mut paths = Vec::new();
        for arg in args {
            if arg.starts_with('-') && arg != "-" {
                for flag in arg.chars().skip(1) {
                    match flag {
                        'r' | 'R' => recursive = true,
                        'f' => force = true,
                        _ => return unsupported("rm", &format!("-{flag}"), &mut stderr).await,
                    }
                }
            } else {
                paths.push(arg);
            }
        }
        if paths.is_empty() {
            if force {
                return CommandResult::success();
            }
            let _ = stderr.write_all(b"rm: missing operand\n").await;
            return CommandResult::new(1);
        }
        let mut exit = 0;
        for path in paths {
            match remove_path(&fs, &path, recursive).await {
                Ok(()) => {}
                Err(err) if force && err.errno() == Errno::ENOENT => {}
                Err(err) => {
                    exit = 1;
                    write_vfs_error(&mut stderr, "rm", &path, err).await;
                }
            }
        }
        CommandResult::new(exit)
    })
}

/// Searches input with Rust regex syntax, not GNU BRE/ERE syntax.
///
/// This is a known GNU grep deviation made to preserve linear-time matching:
/// invalid Rust regexes fail loudly and return grep's GNU error exit code 2.
fn grep(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stdin,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        let mut flags = GrepFlags::default();
        let mut positional = Vec::new();
        for arg in args {
            if arg.starts_with('-') && arg != "-" {
                for flag in arg.chars().skip(1) {
                    match flag {
                        'i' => flags.ignore_case = true,
                        'v' => flags.invert = true,
                        'n' => flags.line_numbers = true,
                        'c' => flags.count = true,
                        'r' | 'R' => flags.recursive = true,
                        _ => {
                            return unsupported_with_code(
                                "grep",
                                &format!("-{flag}"),
                                &mut stderr,
                                2,
                            )
                            .await;
                        }
                    }
                }
            } else {
                positional.push(arg);
            }
        }
        if positional.is_empty() {
            let _ = stderr.write_all(b"grep: missing pattern\n").await;
            return CommandResult::new(2);
        }
        let pattern = positional.remove(0);
        let regex = match RegexBuilder::new(&pattern)
            .case_insensitive(flags.ignore_case)
            .build()
        {
            Ok(regex) => regex,
            Err(err) => {
                let _ = stderr.write_all(format!("grep: {err}\n").as_bytes()).await;
                return CommandResult::new(2);
            }
        };
        let show_path = flags.recursive || positional.len() > 1;
        let mut had_error = false;
        let mut matched_any = false;
        if positional.is_empty() {
            match grep_reader(&mut stdout, &mut stdin, "", &regex, flags, false).await {
                Ok(matched) => matched_any |= matched,
                Err(err) => {
                    if report_stream_error(&mut stderr, "grep", "-", err).await {
                        return CommandResult::new(2);
                    }
                    return CommandResult::failure();
                }
            }
        } else {
            let mut inputs = GrepInputs::new(positional, flags.recursive);
            while let Some(input) = inputs.next(&fs).await {
                let path = match input {
                    Ok(path) => path,
                    Err((path, err)) => {
                        had_error = true;
                        write_vfs_error(&mut stderr, "grep", &path, err).await;
                        continue;
                    }
                };
                let label = if path == "-" {
                    "(standard input)"
                } else {
                    &path
                };
                let result = if path == "-" {
                    grep_reader(&mut stdout, &mut stdin, label, &regex, flags, show_path)
                        .await
                        .map_err(Ok)
                } else {
                    match fs.stream_reader(&path).await {
                        Ok(mut reader) => {
                            grep_reader(&mut stdout, &mut reader, label, &regex, flags, show_path)
                                .await
                                .map_err(Ok)
                        }
                        Err(err) => Err(Err(err)),
                    }
                };
                match result {
                    Ok(matched) => matched_any |= matched,
                    Err(Ok(err)) => {
                        if report_stream_error(&mut stderr, "grep", &path, err).await {
                            had_error = true;
                            continue;
                        }
                        return CommandResult::failure();
                    }
                    Err(Err(err)) => {
                        had_error = true;
                        write_vfs_error(&mut stderr, "grep", &path, err).await;
                    }
                }
            }
        }
        CommandResult::new(if had_error {
            2
        } else if matched_any {
            0
        } else {
            1
        })
    })
}

fn head(ctx: CommandContext) -> CommandFuture {
    boxed(async move { head_tail(ctx, true).await })
}

fn tail(ctx: CommandContext) -> CommandFuture {
    boxed(async move { head_tail(ctx, false).await })
}

fn sort(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            limits,
            mut stdin,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        let mut reverse = false;
        let mut numeric = false;
        let mut unique = false;
        let mut files = Vec::new();
        for arg in args {
            if arg.starts_with('-') && arg != "-" {
                for flag in arg.chars().skip(1) {
                    match flag {
                        'r' => reverse = true,
                        'n' => numeric = true,
                        'u' => unique = true,
                        _ => {
                            return unsupported_with_code(
                                "sort",
                                &format!("-{flag}"),
                                &mut stderr,
                                2,
                            )
                            .await;
                        }
                    }
                }
            } else {
                files.push(arg);
            }
        }
        // Sorting is the one text builtin that must see the full input before
        // it can produce GNU-compatible ordering.
        let input = match read_inputs(
            &fs,
            &files,
            &mut stdin,
            limits.sort_input_bytes,
            "sort",
            &mut stderr,
        )
        .await
        {
            Ok(input) => input,
            Err(()) => return CommandResult::new(2),
        };
        if fs.checkpoint().await.is_err() {
            return CommandResult::new(124);
        }
        let text = String::from_utf8_lossy(&input);
        let mut lines: Vec<_> = text.lines().collect();
        if numeric {
            lines.sort_unstable_by(|a, b| {
                numeric_key(a)
                    .total_cmp(&numeric_key(b))
                    .then_with(|| a.cmp(b))
            });
        } else {
            lines.sort_unstable();
        }
        if unique {
            if numeric {
                lines.dedup_by(|a, b| numeric_key(a).total_cmp(&numeric_key(b)).is_eq());
            } else {
                lines.dedup();
            }
        }
        if reverse {
            lines.reverse();
        }
        for (index, line) in lines.into_iter().enumerate() {
            if index.is_multiple_of(1024) && fs.checkpoint().await.is_err() {
                return CommandResult::new(124);
            }
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
            {
                return CommandResult::failure();
            }
        }
        CommandResult::success()
    })
}

fn uniq(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stdin,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        let mut count = false;
        let mut repeated = false;
        let mut unique_only = false;
        let mut files = Vec::new();
        for arg in args {
            match arg.as_str() {
                "-c" => count = true,
                "-d" => repeated = true,
                "-u" => unique_only = true,
                flag if flag.starts_with('-') => {
                    return unsupported("uniq", flag, &mut stderr).await;
                }
                _ => files.push(arg),
            }
        }
        if files.len() > 1 {
            let _ = stderr.write_all(b"uniq: extra operand\n").await;
            return CommandResult::new(1);
        }
        let result = if files.is_empty() || files[0] == "-" {
            uniq_reader(&mut stdin, &mut stdout, count, repeated, unique_only)
                .await
                .map_err(Ok)
        } else {
            match fs.stream_reader(&files[0]).await {
                Ok(mut reader) => {
                    uniq_reader(&mut reader, &mut stdout, count, repeated, unique_only)
                        .await
                        .map_err(Ok)
                }
                Err(err) => Err(Err(err)),
            }
        };
        if let Err(err) = result {
            let path = files.first().map_or("-", String::as_str);
            match err {
                Ok(err) => {
                    if !report_stream_error(&mut stderr, "uniq", path, err).await {
                        return CommandResult::failure();
                    }
                }
                Err(err) => write_vfs_error(&mut stderr, "uniq", path, err).await,
            }
            return CommandResult::new(1);
        }
        CommandResult::success()
    })
}

fn wc(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stdin,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        let mut show_lines = false;
        let mut show_words = false;
        let mut show_bytes = false;
        let mut files = Vec::new();
        for arg in args {
            if arg.starts_with('-') && arg != "-" {
                for flag in arg.chars().skip(1) {
                    match flag {
                        'l' => show_lines = true,
                        'w' => show_words = true,
                        'c' => show_bytes = true,
                        _ => return unsupported("wc", &format!("-{flag}"), &mut stderr).await,
                    }
                }
            } else {
                files.push(arg);
            }
        }
        if !show_lines && !show_words && !show_bytes {
            show_lines = true;
            show_words = true;
            show_bytes = true;
        }
        let mut total = Counts::default();
        if files.is_empty() {
            let counts = match counts_reader(&mut stdin).await {
                Ok(counts) => counts,
                Err(err) => {
                    if report_stream_error(&mut stderr, "wc", "-", err).await {
                        return CommandResult::new(1);
                    }
                    return CommandResult::failure();
                }
            };
            write_counts(
                &mut stdout,
                counts,
                None,
                show_lines,
                show_words,
                show_bytes,
                7,
            )
            .await;
        } else {
            let mut rows = Vec::new();
            let mut exit = 0;
            for path in &files {
                let counts = if path == "-" {
                    counts_reader(&mut stdin).await.map_err(Ok)
                } else {
                    match fs.stream_reader(path).await {
                        Ok(mut reader) => counts_reader(&mut reader).await.map_err(Ok),
                        Err(err) => Err(Err(err)),
                    }
                };
                match counts {
                    Ok(counts) => {
                        total += counts;
                        rows.push((counts, path.as_str()));
                    }
                    Err(Ok(err)) => {
                        if report_stream_error(&mut stderr, "wc", path, err).await {
                            exit = 1;
                        } else {
                            return CommandResult::failure();
                        }
                    }
                    Err(Err(err)) => {
                        exit = 1;
                        write_vfs_error(&mut stderr, "wc", path, err).await;
                    }
                }
            }
            if files.len() > 1 {
                rows.push((total, "total"));
            }
            // GNU wc uses width 7 only for stdin-only output; named files use
            // the natural width of the largest printed count.
            let width = rows
                .iter()
                .flat_map(|(counts, _)| {
                    selected_counts(*counts, show_lines, show_words, show_bytes)
                })
                .map(decimal_width)
                .max()
                .unwrap_or(1);
            for (counts, name) in rows {
                write_counts(
                    &mut stdout,
                    counts,
                    Some(name),
                    show_lines,
                    show_words,
                    show_bytes,
                    width,
                )
                .await;
            }
            return CommandResult::new(exit);
        }
        CommandResult::success()
    })
}

/// Applies `s///` substitutions with Rust regex syntax, not GNU BRE syntax.
fn sed(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stdin,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        if args.is_empty() {
            let _ = stderr.write_all(b"sed: missing command\n").await;
            return CommandResult::new(1);
        }
        let script = &args[0];
        let Some(sub) = parse_sed_substitution(script) else {
            let _ = stderr
                .write_all(
                    b"sed: unsupported command; tinysandbox supports s/// with g and i flags\n",
                )
                .await;
            return CommandResult::new(1);
        };
        let regex = match RegexBuilder::new(&sub.pattern)
            .case_insensitive(sub.ignore_case)
            .build()
        {
            Ok(regex) => regex,
            Err(err) => {
                let _ = stderr.write_all(format!("sed: {err}\n").as_bytes()).await;
                return CommandResult::new(1);
            }
        };
        if let Err(index) = validate_sed_replacement(&sub.replacement, regex.captures_len()) {
            let _ = stderr
                .write_all(
                    format!("sed: invalid reference \\{index} on `s' command's RHS\n").as_bytes(),
                )
                .await;
            return CommandResult::new(1);
        }
        let files = args[1..].to_vec();
        if files.is_empty() {
            if let Err(err) = sed_reader(&mut stdin, &mut stdout, &regex, &sub).await {
                if report_stream_error(&mut stderr, "sed", "-", err).await {
                    return CommandResult::new(2);
                }
                return CommandResult::failure();
            }
        } else {
            for file in files {
                let result = if file == "-" {
                    sed_reader(&mut stdin, &mut stdout, &regex, &sub)
                        .await
                        .map_err(Ok)
                } else {
                    match fs.stream_reader(&file).await {
                        Ok(mut reader) => sed_reader(&mut reader, &mut stdout, &regex, &sub)
                            .await
                            .map_err(Ok),
                        Err(err) => Err(Err(err)),
                    }
                };
                if let Err(err) = result {
                    match err {
                        Ok(err) => {
                            if !report_stream_error(&mut stderr, "sed", &file, err).await {
                                return CommandResult::failure();
                            }
                        }
                        Err(err) => write_vfs_error(&mut stderr, "sed", &file, err).await,
                    }
                    return CommandResult::new(2);
                }
            }
        }
        CommandResult::success()
    })
}

fn stat(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            fs,
            mut stdout,
            mut stderr,
            ..
        } = ctx;
        if args.is_empty() {
            let _ = stderr.write_all(b"stat: missing operand\n").await;
            return CommandResult::new(1);
        }
        let mut exit = 0;
        for path in args {
            match fs.stat(&path).await {
                Ok(metadata) => {
                    let kind = if metadata.file_type == FileType::Directory {
                        "directory"
                    } else {
                        "regular file"
                    };
                    let _ = stdout
                        .write_all(
                            format!("  File: {path}\n  Size: {}\n  Type: {kind}\n", metadata.len)
                                .as_bytes(),
                        )
                        .await;
                }
                Err(err) => {
                    exit = 1;
                    write_vfs_error(&mut stderr, "stat", &path, err).await;
                }
            }
        }
        CommandResult::new(exit)
    })
}

async fn head_tail(ctx: CommandContext, head_mode: bool) -> CommandResult {
    let CommandContext {
        args,
        fs,
        limits,
        mut stdin,
        mut stdout,
        mut stderr,
        ..
    } = ctx;
    let cmd = if head_mode { "head" } else { "tail" };
    let mut n = TailCount::Last(10);
    let mut verbose = false;
    let mut files = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-v" => {
                verbose = true;
            }
            "-n" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    let _ = stderr
                        .write_all(
                            format!("{cmd}: option requires an argument -- 'n'\n").as_bytes(),
                        )
                        .await;
                    return CommandResult::new(1);
                };
                match parse_head_tail_count(value, head_mode) {
                    Ok(value) => n = value,
                    Err(_) => {
                        let _ = stderr
                            .write_all(
                                format!("{cmd}: invalid number of lines: '{value}'\n").as_bytes(),
                            )
                            .await;
                        return CommandResult::new(1);
                    }
                }
            }
            flag if flag.starts_with('-') => return unsupported(cmd, flag, &mut stderr).await,
            path => files.push(path.to_owned()),
        }
        i += 1;
    }
    let show_headers = verbose || files.len() > 1;
    let settings = HeadTailSettings {
        count: n,
        head_mode,
        window_bytes: limits.tail_input_bytes,
    };
    if files.is_empty() {
        if let Err(err) = head_tail_reader(
            &mut stdin,
            &mut stdout,
            "standard input",
            false,
            settings,
            0,
        )
        .await
        {
            if report_stream_error(&mut stderr, cmd, "-", err).await {
                return CommandResult::new(1);
            }
            return CommandResult::failure();
        }
    } else {
        for (index, file) in files.iter().enumerate() {
            let result = if file == "-" {
                head_tail_reader(&mut stdin, &mut stdout, file, show_headers, settings, index).await
            } else {
                match fs.stream_reader(file).await {
                    Ok(mut reader) => {
                        head_tail_reader(
                            &mut reader,
                            &mut stdout,
                            file,
                            show_headers,
                            settings,
                            index,
                        )
                        .await
                    }
                    Err(err) => {
                        write_vfs_error(&mut stderr, cmd, file, err).await;
                        return CommandResult::new(1);
                    }
                }
            };
            if let Err(err) = result {
                if report_stream_error(&mut stderr, cmd, file, err).await {
                    return CommandResult::new(1);
                }
                return CommandResult::failure();
            }
        }
    }
    CommandResult::success()
}

async fn head_tail_reader(
    reader: &mut BoxAsyncRead,
    stdout: &mut BoxAsyncWrite,
    label: &str,
    show_header: bool,
    settings: HeadTailSettings,
    index: usize,
) -> io::Result<()> {
    let HeadTailSettings {
        count,
        head_mode,
        window_bytes,
    } = settings;
    if show_header {
        if index > 0 {
            stdout.write_all(b"\n").await?;
        }
        stdout
            .write_all(format!("==> {label} <==\n").as_bytes())
            .await?;
    }

    let mut pending = LineBuffer::default();
    match count {
        TailCount::Last(limit) if head_mode => {
            for _ in 0..limit {
                let Some(line) = read_line(reader, &mut pending).await? else {
                    break;
                };
                stdout.write_all(&line).await?;
            }
        }
        TailCount::Last(limit) => {
            let mut window = VecDeque::new();
            let mut retained_lines = 0usize;
            while let Some(line) = read_line(reader, &mut pending).await? {
                if limit > 0 {
                    if retained_lines == limit {
                        while let Some(byte) = window.pop_front() {
                            if byte == b'\n' {
                                break;
                            }
                        }
                        retained_lines -= 1;
                    }
                    if window.len().saturating_add(line.len()) > window_bytes {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "retained input too large for tinysandbox tail",
                        ));
                    }
                    window.extend(line);
                    retained_lines += 1;
                }
            }
            let (front, back) = window.as_slices();
            stdout.write_all(front).await?;
            stdout.write_all(back).await?;
        }
        TailCount::From(start) => {
            let mut line_no = 1_usize;
            while let Some(line) = read_line(reader, &mut pending).await? {
                if line_no >= start {
                    stdout.write_all(&line).await?;
                }
                line_no += 1;
            }
        }
    }
    Ok(())
}

async fn unsupported(cmd: &str, flag: &str, stderr: &mut BoxAsyncWrite) -> CommandResult {
    unsupported_with_code(cmd, flag, stderr, 1).await
}

async fn unsupported_with_code(
    cmd: &str,
    flag: &str,
    stderr: &mut BoxAsyncWrite,
    exit_code: i32,
) -> CommandResult {
    let _ = stderr
        .write_all(format!("{cmd}: unsupported option '{flag}'\n").as_bytes())
        .await;
    CommandResult::new(exit_code)
}

fn unsupported_flag(args: &[String]) -> Option<&str> {
    args.iter()
        .find(|arg| arg.starts_with('-') && arg.as_str() != "-")
        .map(String::as_str)
}

async fn write_vfs_error(stderr: &mut BoxAsyncWrite, cmd: &str, path: &str, err: VfsError) {
    let _ = stderr
        .write_all(format!("{cmd}: {path}: {}\n", errno_message(err.errno())).as_bytes())
        .await;
}

async fn write_line_too_long(stderr: &mut BoxAsyncWrite, cmd: &str, path: &str) {
    let _ = stderr
        .write_all(format!("{cmd}: {path}: {LINE_TOO_LONG}\n").as_bytes())
        .await;
}

async fn report_stream_error(
    stderr: &mut BoxAsyncWrite,
    cmd: &str,
    path: &str,
    err: io::Error,
) -> bool {
    if is_broken_pipe(&err) {
        return false;
    }
    if is_line_too_long(&err) {
        write_line_too_long(stderr, cmd, path).await;
        return true;
    }
    if err.kind() == io::ErrorKind::InvalidData {
        let _ = stderr
            .write_all(format!("{cmd}: {path}: {err}\n").as_bytes())
            .await;
        return true;
    }
    if let Some(err) = io_error_to_vfs(err) {
        write_vfs_error(stderr, cmd, path, err).await;
        return true;
    }
    false
}

fn io_error_to_vfs(err: io::Error) -> Option<VfsError> {
    if is_broken_pipe(&err) {
        None
    } else {
        Some(
            err.get_ref()
                .and_then(|source| source.downcast_ref::<VfsError>())
                .copied()
                .unwrap_or_else(|| VfsError::new(Errno::EINVAL)),
        )
    }
}

fn is_broken_pipe(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::BrokenPipe
}

fn line_too_long_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, LINE_TOO_LONG)
}

fn is_line_too_long(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::InvalidData && err.to_string() == LINE_TOO_LONG
}

fn parse_jq_args(args: Vec<String>) -> Result<JqOptions, String> {
    let mut raw_output = false;
    let mut join_output = false;
    let mut compact_output = false;
    let mut exit_status = false;
    let mut null_input = false;
    let mut slurp = false;
    let mut sort_keys = false;
    let mut indent = "  ".to_owned();
    let mut vars = Vec::new();
    let mut filter = None;
    let mut files = Vec::new();
    let mut options_done = false;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if filter.is_some() {
            files.extend(args[i..].iter().cloned());
            break;
        }

        if options_done || !arg.starts_with('-') || arg == "-" {
            filter = Some(arg.clone());
            i += 1;
            continue;
        }

        match arg.as_str() {
            "--" => {
                options_done = true;
                i += 1;
            }
            "--tab" => {
                indent = "\t".to_owned();
                i += 1;
            }
            "--indent" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err("jq: option --indent requires an argument\n".to_owned());
                };
                indent = parse_jq_indent(value)?;
                i += 1;
            }
            "--arg" => {
                let (name, value) = parse_jq_arg_pair(&args, i, "--arg")?;
                vars.push(JqVariable {
                    name: format!("${name}"),
                    value: jaq_json::Val::utf8_str(value.to_owned()),
                });
                i += 3;
            }
            "--argjson" => {
                let (name, value) = parse_jq_arg_pair(&args, i, "--argjson")?;
                validate_jq_json_nesting(value.as_bytes())
                    .map_err(|err| format!("jq: invalid JSON for --argjson {name}: {err}\n"))?;
                let value = jaq_json::read::parse_single(value.as_bytes())
                    .map_err(|err| format!("jq: invalid JSON for --argjson {name}: {err}\n"))?;
                vars.push(JqVariable {
                    name: format!("${name}"),
                    value,
                });
                i += 3;
            }
            flag if flag.starts_with("--") => {
                return Err(format!("jq: unsupported option '{flag}'\n"));
            }
            flags => {
                for flag in flags.chars().skip(1) {
                    match flag {
                        'r' => raw_output = true,
                        'j' => {
                            raw_output = true;
                            join_output = true;
                        }
                        'c' => compact_output = true,
                        'e' => exit_status = true,
                        'n' => null_input = true,
                        's' => slurp = true,
                        'S' => sort_keys = true,
                        _ => return Err(format!("jq: unsupported option '-{flag}'\n")),
                    }
                }
                i += 1;
            }
        }
    }

    let Some(filter) = filter else {
        return Err("jq: missing filter\n".to_owned());
    };

    if indent.is_empty() {
        compact_output = true;
    }

    Ok(JqOptions {
        filter,
        files,
        raw_output,
        join_output,
        compact_output,
        exit_status,
        null_input,
        slurp,
        sort_keys,
        indent,
        vars,
    })
}

fn parse_jq_arg_pair<'a>(
    args: &'a [String],
    option_index: usize,
    option: &str,
) -> Result<(&'a str, &'a str), String> {
    let Some(name) = args.get(option_index + 1) else {
        return Err(format!("jq: option {option} requires a name\n"));
    };
    let Some(value) = args.get(option_index + 2) else {
        return Err(format!("jq: option {option} requires a value\n"));
    };
    if !is_jq_var_name(name) {
        return Err(format!("jq: invalid variable name '{name}'\n"));
    }
    Ok((name, value))
}

fn is_jq_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn parse_jq_indent(value: &str) -> Result<String, String> {
    let n = value
        .parse::<usize>()
        .map_err(|_| format!("jq: invalid indent '{value}'\n"))?;
    if n > 8 {
        return Err(format!("jq: invalid indent '{value}'\n"));
    }
    Ok(" ".repeat(n))
}

async fn read_jq_inputs(
    fs: &Fs,
    files: &[String],
    stdin: &mut BoxAsyncRead,
    limit: usize,
) -> Result<Vec<JqInputSource>, JqInputError> {
    let mut inputs = Vec::new();
    let mut total = 0;
    if files.is_empty() {
        let mut data = Vec::new();
        read_jq_reader(stdin, limit, &mut total, &mut data)
            .await
            .map_err(|err| JqInputError::Read {
                path: "-".to_owned(),
                err,
            })?;
        push_jq_input(&mut inputs, "-".to_owned(), data)?;
        return Ok(inputs);
    }

    for file in files {
        if file == "-" {
            let mut data = Vec::new();
            read_jq_reader(stdin, limit, &mut total, &mut data)
                .await
                .map_err(|err| JqInputError::Read {
                    path: file.clone(),
                    err,
                })?;
            push_jq_input(&mut inputs, file.clone(), data)?;
        } else {
            let mut reader = fs
                .stream_reader(file)
                .await
                .map_err(|err| JqInputError::Vfs {
                    path: file.clone(),
                    err,
                })?;
            let mut data = Vec::new();
            read_jq_reader(&mut reader, limit, &mut total, &mut data)
                .await
                .map_err(|err| JqInputError::Read {
                    path: file.clone(),
                    err,
                })?;
            push_jq_input(&mut inputs, file.clone(), data)?;
        }
    }

    Ok(inputs)
}

fn push_jq_input(
    inputs: &mut Vec<JqInputSource>,
    path: String,
    data: Vec<u8>,
) -> Result<(), JqInputError> {
    validate_jq_json_nesting(&data).map_err(|err| JqInputError::JsonDepth {
        path: path.clone(),
        err,
    })?;
    inputs.push(JqInputSource { path, data });
    Ok(())
}

fn validate_jq_json_nesting(data: &[u8]) -> Result<(), JqJsonDepthError> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut in_comment = false;

    for &byte in data {
        if in_comment {
            if byte == b'\n' {
                in_comment = false;
            }
            continue;
        }

        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match byte {
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'#' => in_comment = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > MAX_JQ_JSON_NESTING {
                    return Err(JqJsonDepthError {
                        max: MAX_JQ_JSON_NESTING,
                    });
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    Ok(())
}

async fn read_jq_reader(
    reader: &mut BoxAsyncRead,
    limit: usize,
    total: &mut usize,
    input: &mut Vec<u8>,
) -> io::Result<()> {
    let mut buf = [0_u8; 64 * 1024];
    loop {
        tokio::task::yield_now().await;
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        if total.saturating_add(n) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "input too large for tinysandbox jq",
            ));
        }
        *total += n;
        input.extend_from_slice(&buf[..n]);
    }
}

async fn write_jq_input_error(stderr: &mut BoxAsyncWrite, err: JqInputError) {
    match err {
        JqInputError::Vfs { path, err } => write_vfs_error(stderr, "jq", &path, err).await,
        JqInputError::JsonDepth { path, err } => {
            let _ = stderr
                .write_all(format!("jq: {path}: {err}\n").as_bytes())
                .await;
        }
        JqInputError::Read { path, err } if err.kind() == io::ErrorKind::InvalidData => {
            let _ = stderr
                .write_all(format!("jq: {path}: {}\n", err).as_bytes())
                .await;
        }
        JqInputError::Read { path, err } => {
            if !report_stream_error(stderr, "jq", &path, err).await {
                let _ = stderr.write_all(b"jq: read error\n").await;
            }
        }
    }
}

fn run_jq_program(
    options: JqOptions,
    inputs: Vec<JqInputSource>,
    tx: mpsc::Sender<JqStreamMessage>,
    deadline: Option<Instant>,
    cancelled: Arc<AtomicBool>,
) {
    let _guard = jq::set_control(deadline, cancelled);
    let done = run_jq_program_inner(&options, inputs, &tx);
    let _ = tx.blocking_send(JqStreamMessage::Done(done));
}

fn run_jq_program_inner(
    options: &JqOptions,
    inputs: Vec<JqInputSource>,
    tx: &mpsc::Sender<JqStreamMessage>,
) -> JqRunDone {
    let global_vars: Vec<_> = options.vars.iter().map(|var| var.name.clone()).collect();
    let program = match jq::compile_with_vars(&options.filter, &global_vars) {
        Ok(program) => program,
        Err(err) => return jq_error_outcome(err),
    };

    let vars: Vec<_> = options.vars.iter().map(|var| var.value.clone()).collect();
    let mut last_output = None;

    if options.null_input {
        if let Err(done) = run_jq_input_value(
            &program,
            jaq_json::Val::Null,
            &vars,
            options,
            tx,
            &mut last_output,
        ) {
            return done;
        }
    } else {
        if options.slurp {
            let mut values = Vec::new();
            for source in inputs {
                for value in jaq_json::read::parse_many(&source.data) {
                    match value {
                        Ok(value) => values.push(value),
                        Err(err) => return jq_parse_error_outcome(&source.path, err),
                    }
                }
            }
            if let Err(done) = run_jq_input_value(
                &program,
                jaq_json::Val::Arr(values.into()),
                &vars,
                options,
                tx,
                &mut last_output,
            ) {
                return done;
            }
        } else {
            for source in inputs {
                for value in jaq_json::read::parse_many(&source.data) {
                    let value = match value {
                        Ok(value) => value,
                        Err(err) => return jq_parse_error_outcome(&source.path, err),
                    };
                    if let Err(done) =
                        run_jq_input_value(&program, value, &vars, options, tx, &mut last_output)
                    {
                        return done;
                    }
                }
            }
        }
    }

    JqRunDone {
        exit_code: jq_exit_code(options.exit_status, last_output),
        stderr: Vec::new(),
    }
}

fn run_jq_input_value(
    program: &jq::JqProgram,
    input: jaq_json::Val,
    vars: &[jaq_json::Val],
    options: &JqOptions,
    tx: &mpsc::Sender<JqStreamMessage>,
    last_output: &mut Option<bool>,
) -> Result<(), JqRunDone> {
    for value in program.output_iter(input, vars) {
        let value = value.map_err(jq_error_outcome)?;
        *last_output = Some(jq_truthy(&value));
        let mut writer = JqOutputWriter {
            tx,
            pending: Vec::new(),
        };
        write_jq_value(&mut writer, &value, options)
            .and_then(|()| io::Write::flush(&mut writer))
            .map_err(|err| JqRunDone {
                exit_code: if is_broken_pipe(&err) { 1 } else { 5 },
                stderr: if is_broken_pipe(&err) {
                    Vec::new()
                } else {
                    format!("jq: output error: {err}\n").into_bytes()
                },
            })?;
    }
    Ok(())
}

fn jq_exit_code(exit_status: bool, last_output: Option<bool>) -> i32 {
    if exit_status {
        match last_output {
            Some(true) => 0,
            Some(false) => 1,
            None => 4,
        }
    } else {
        0
    }
}

// Bound serialization buffers even when one evaluator value is very large.
// This does not impose a heap limit on jaq's own evaluation; its documented
// cooperative evaluator limit still applies.
struct JqOutputWriter<'a> {
    tx: &'a mpsc::Sender<JqStreamMessage>,
    pending: Vec<u8>,
}

impl io::Write for JqOutputWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = bytes.len().min(STREAM_CHUNK_BYTES - self.pending.len());
        self.pending.extend_from_slice(&bytes[..n]);
        if self.pending.len() == STREAM_CHUNK_BYTES {
            self.flush()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            let chunk = std::mem::take(&mut self.pending);
            self.tx
                .blocking_send(JqStreamMessage::Stdout(chunk))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "jq output closed"))?;
        }
        Ok(())
    }
}

fn write_jq_value(
    out: &mut impl io::Write,
    value: &jaq_json::Val,
    options: &JqOptions,
) -> io::Result<()> {
    if options.raw_output {
        match value {
            jaq_json::Val::TStr(bytes) | jaq_json::Val::BStr(bytes) => {
                out.write_all(bytes)?;
            }
            _ => write_jq_json(out, value, options)?,
        }
    } else {
        write_jq_json(out, value, options)?;
    }
    if !options.join_output {
        out.write_all(b"\n")?;
    }
    Ok(())
}

fn write_jq_json(
    out: &mut impl io::Write,
    value: &jaq_json::Val,
    options: &JqOptions,
) -> io::Result<()> {
    let pp = jaq_json::write::Pp {
        indent: (!options.compact_output).then(|| options.indent.clone()),
        sort_keys: options.sort_keys,
        sep_space: !options.compact_output,
        ..Default::default()
    };
    jaq_json::write::write(out, &pp, 0, value)
}

fn jq_truthy(value: &jaq_json::Val) -> bool {
    !matches!(value, jaq_json::Val::Null | jaq_json::Val::Bool(false))
}

fn jq_parse_error_outcome(path: &str, err: jaq_json::read::Error) -> JqRunDone {
    JqRunDone {
        exit_code: 5,
        stderr: format!("jq: {path}: parse error: {err}\n").into_bytes(),
    }
}

fn jq_error_outcome(err: JqError) -> JqRunDone {
    let exit_code = match err {
        JqError::Compile(_) => 3,
        JqError::Runtime(_) => 5,
        JqError::Halt(code) => code,
    };
    JqRunDone {
        exit_code,
        stderr: format!("jq: {err}\n").into_bytes(),
    }
}

async fn write_ls_entry(stdout: &mut BoxAsyncWrite, name: &str, metadata: Metadata, long: bool) {
    if long {
        let mode = if metadata.file_type == FileType::Directory {
            "drwxr-xr-x"
        } else {
            "-rw-r--r--"
        };
        let _ = stdout
            .write_all(format!("{mode} 1 0 0 {:>6} Jan  1 00:00 {name}\n", metadata.len).as_bytes())
            .await;
    } else {
        let _ = stdout.write_all(format!("{name}\n").as_bytes()).await;
    }
}

async fn list_dir(fs: &Fs, path: &str, all: bool) -> Result<Vec<(String, Metadata)>, VfsError> {
    let mut entries: Vec<_> = fs
        .readdir(path)
        .await?
        .into_iter()
        .filter(|entry| all || !entry.name.starts_with('.'))
        .map(|entry| (entry.name, entry.metadata))
        .collect();
    if all {
        entries.insert(
            0,
            (
                "..".to_owned(),
                Metadata {
                    file_type: FileType::Directory,
                    len: 0,
                },
            ),
        );
        entries.insert(
            0,
            (
                ".".to_owned(),
                Metadata {
                    file_type: FileType::Directory,
                    len: 0,
                },
            ),
        );
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

async fn mkdir_p(fs: &Fs, path: &str) -> Result<(), VfsError> {
    let path = fs.resolve(path);
    let mut current = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        current.push('/');
        current.push_str(part);
        ensure_directory(fs, &current).await?;
    }
    Ok(())
}

async fn parse_recursive_args(
    cmd: &str,
    args: Vec<String>,
    stderr: &mut BoxAsyncWrite,
) -> Result<(bool, Vec<String>), CommandResult> {
    let mut recursive = false;
    let mut paths = Vec::new();
    for arg in args {
        if arg.starts_with('-') && arg != "-" {
            for flag in arg.chars().skip(1) {
                match flag {
                    'r' | 'R' => recursive = true,
                    _ => return Err(unsupported(cmd, &format!("-{flag}"), stderr).await),
                }
            }
        } else {
            paths.push(arg);
        }
    }
    Ok((recursive, paths))
}

async fn destination_for(fs: &Fs, source: &str, dest: &str) -> String {
    if matches!(fs.stat(dest).await, Ok(meta) if meta.file_type == FileType::Directory) {
        join_path(dest, &basename(source))
    } else {
        dest.to_owned()
    }
}

async fn copy_path(fs: &Fs, source: &str, dest: &str, recursive: bool) -> Result<(), VfsError> {
    let source = fs.resolve(source);
    let dest = fs.resolve(dest);
    let metadata = fs.stat(&source).await?;
    // Check the effective, normalized target before mkdir/open can mutate it.
    // The opened-handle VFS interface has no inode identity; backend aliases
    // such as host-created hard links remain part of the backend contract.
    if source == dest
        || (metadata.file_type == FileType::Directory
            && (source == "/" || dest.starts_with(&format!("{source}/"))))
    {
        return Err(VfsError::new(Errno::EINVAL));
    }
    if metadata.file_type != FileType::Directory {
        return copy_file(fs, &source, &dest).await;
    }
    if !recursive {
        return Err(VfsError::new(Errno::EISDIR));
    }
    ensure_directory(fs, &dest).await?;

    // Keep one directory iterator per depth rather than recursive futures or
    // a worklist of the entire tree. Fs enforces the shared path-depth limit.
    let entries = fs.readdir(&source).await?.into_iter();
    let mut stack = vec![(source, dest, entries)];
    while let Some((source, dest, entries)) = stack.last_mut() {
        fs.checkpoint().await?;
        let Some(entry) = entries.next() else {
            stack.pop();
            continue;
        };
        let child_source = join_path(source, &entry.name);
        let child_dest = join_path(dest, &entry.name);
        if entry.metadata.file_type == FileType::Directory {
            ensure_directory(fs, &child_dest).await?;
            let entries = fs.readdir(&child_source).await?.into_iter();
            stack.push((child_source, child_dest, entries));
        } else {
            copy_file(fs, &child_source, &child_dest).await?;
        }
    }
    Ok(())
}

async fn ensure_directory(fs: &Fs, path: &str) -> Result<(), VfsError> {
    match fs.mkdir(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.errno() == Errno::EEXIST => {
            if fs.stat(path).await?.file_type == FileType::Directory {
                Ok(())
            } else {
                Err(VfsError::new(Errno::ENOTDIR))
            }
        }
        Err(err) => Err(err),
    }
}

async fn copy_file(fs: &Fs, source: &str, dest: &str) -> Result<(), VfsError> {
    let source_handle = fs.open(source, OpenMode::read_only()).await?;
    let dest_handle = match fs
        .open(dest, OpenMode::write_only().create().truncate())
        .await
    {
        Ok(handle) => handle,
        Err(err) => {
            let _ = fs.close(source_handle).await;
            return Err(err);
        }
    };
    let result = async {
        let mut offset = 0u64;
        let mut buf = vec![0; STREAM_CHUNK_BYTES];
        loop {
            fs.checkpoint().await?;
            let (bytes, n) = fs.read_at(source_handle, offset, buf).await?;
            buf = bytes;
            if n == 0 {
                return Ok(());
            }
            if n > buf.len() {
                return Err(VfsError::new(Errno::EIO));
            }
            let mut written = 0;
            while written < n {
                fs.checkpoint().await?;
                let count = fs
                    .write_at(
                        dest_handle,
                        offset + written as u64,
                        buf[written..n].to_vec(),
                    )
                    .await?;
                if count == 0 || count > n - written {
                    return Err(VfsError::new(Errno::EIO));
                }
                written += count;
            }
            offset = offset
                .checked_add(n as u64)
                .ok_or_else(|| VfsError::new(Errno::EFBIG))?;
        }
    }
    .await;
    let result = result.and(fs.close(source_handle).await);
    if let Err(err) = result {
        // A staged destination must not commit a partial object on failure.
        let _ = fs.abort(dest_handle).await;
        return Err(err);
    }
    fs.close(dest_handle).await
}

async fn remove_path(fs: &Fs, path: &str, recursive: bool) -> Result<(), VfsError> {
    let metadata = fs.stat(path).await?;
    if metadata.file_type != FileType::Directory {
        return fs.unlink(path).await;
    }
    if !recursive {
        return Err(VfsError::new(Errno::EISDIR));
    }
    let mut stack = vec![(path.to_owned(), fs.readdir(path).await?.into_iter())];
    while let Some((path, entries)) = stack.last_mut() {
        fs.checkpoint().await?;
        let Some(entry) = entries.next() else {
            let (path, _) = stack.pop().expect("directory frame exists");
            fs.rmdir(&path).await?;
            continue;
        };
        let child = join_path(path, &entry.name);
        if entry.metadata.file_type == FileType::Directory {
            let entries = fs.readdir(&child).await?.into_iter();
            stack.push((child, entries));
        } else {
            fs.unlink(&child).await?;
        }
    }
    Ok(())
}

// Discover the next file only when grep is ready to consume it. Retaining an
// eager list of every path made a wide tree an unnecessary working-memory cost.
struct GrepInputs {
    roots: std::vec::IntoIter<String>,
    directories: Vec<(String, std::vec::IntoIter<DirEntry>)>,
    recursive: bool,
}

impl GrepInputs {
    fn new(paths: Vec<String>, recursive: bool) -> Self {
        Self {
            roots: paths.into_iter(),
            directories: Vec::new(),
            recursive,
        }
    }

    async fn next(&mut self, fs: &Fs) -> Option<Result<String, (String, VfsError)>> {
        loop {
            let path = if let Some((parent, entries)) = self.directories.last_mut() {
                match entries.next() {
                    Some(entry) => join_path(parent, &entry.name),
                    None => {
                        self.directories.pop();
                        continue;
                    }
                }
            } else {
                self.roots.next()?
            };
            if let Err(err) = fs.checkpoint().await {
                self.roots = Vec::new().into_iter();
                self.directories.clear();
                return Some(Err((path, err)));
            }
            if path == "-" {
                return Some(Ok(path));
            }
            let metadata = match fs.stat(&path).await {
                Ok(metadata) => metadata,
                Err(err) => return Some(Err((path, err))),
            };
            if !self.recursive || metadata.file_type != FileType::Directory {
                return Some(Ok(path));
            }
            match fs.readdir(&path).await {
                Ok(entries) => self.directories.push((path, entries.into_iter())),
                Err(err) => return Some(Err((path, err))),
            }
        }
    }
}

async fn grep_reader(
    stdout: &mut BoxAsyncWrite,
    reader: &mut BoxAsyncRead,
    path: &str,
    regex: &Regex,
    flags: GrepFlags,
    show_path: bool,
) -> io::Result<bool> {
    let mut pending = LineBuffer::default();
    let mut matched = 0_usize;
    let mut line_no = 0_usize;
    while let Some(line) = read_line(reader, &mut pending).await? {
        line_no += 1;
        let line = line_text_lossy(&line);
        let is_match = regex.is_match(&line) ^ flags.invert;
        if !is_match {
            continue;
        }
        matched += 1;
        if flags.count {
            continue;
        }
        if show_path {
            stdout.write_all(format!("{path}:").as_bytes()).await?;
        }
        if flags.line_numbers {
            stdout.write_all(format!("{line_no}:").as_bytes()).await?;
        }
        stdout.write_all(format!("{line}\n").as_bytes()).await?;
    }
    if flags.count {
        if show_path {
            stdout.write_all(format!("{path}:").as_bytes()).await?;
        }
        stdout.write_all(format!("{matched}\n").as_bytes()).await?;
    }
    Ok(matched > 0)
}

async fn read_inputs(
    fs: &Fs,
    files: &[String],
    stdin: &mut BoxAsyncRead,
    limit: usize,
    cmd: &str,
    stderr: &mut BoxAsyncWrite,
) -> Result<Vec<u8>, ()> {
    let mut input = Vec::new();
    let default_files = ["-".to_owned()];
    for file in if files.is_empty() {
        &default_files[..]
    } else {
        files
    } {
        let result = if file == "-" {
            read_input_bounded(fs, stdin, &mut input, limit).await
        } else {
            match fs.stream_reader(file).await {
                Ok(mut reader) => read_input_bounded(fs, &mut reader, &mut input, limit).await,
                Err(err) => Err(io::Error::other(err)),
            }
        };
        if let Err(err) = result {
            if err.kind() == io::ErrorKind::InvalidData {
                let _ = stderr
                    .write_all(format!("{cmd}: input too large for tinysandbox {cmd}\n").as_bytes())
                    .await;
            } else {
                report_stream_error(stderr, cmd, file, err).await;
            }
            return Err(());
        }
    }
    Ok(input)
}

async fn read_input_bounded(
    fs: &Fs,
    reader: &mut BoxAsyncRead,
    input: &mut Vec<u8>,
    limit: usize,
) -> io::Result<()> {
    let mut buf = [0u8; STREAM_CHUNK_BYTES];
    loop {
        fs.checkpoint().await.map_err(io::Error::other)?;
        // Probe one byte past the remaining budget, never materialize a file
        // before admission, and share the budget across every input source.
        let want = buf
            .len()
            .min(limit.saturating_sub(input.len()).saturating_add(1));
        let n = reader.read(&mut buf[..want]).await?;
        if n == 0 {
            return Ok(());
        }
        if n > limit.saturating_sub(input.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "input too large",
            ));
        }
        input.extend_from_slice(&buf[..n]);
    }
}

#[derive(Default)]
struct LineBuffer {
    bytes: Vec<u8>,
    start: usize,
    scanned: usize,
    eof: bool,
}

async fn read_line(
    reader: &mut BoxAsyncRead,
    pending: &mut LineBuffer,
) -> io::Result<Option<Vec<u8>>> {
    loop {
        if let Some(pos) = pending.bytes[pending.scanned..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = pending.scanned + pos + 1;
            if end - pending.start > MAX_STREAM_LINE_BYTES {
                return Err(line_too_long_error());
            }
            let line = pending.bytes[pending.start..end].to_vec();
            pending.start = end;
            pending.scanned = end;
            return Ok(Some(line));
        }
        pending.scanned = pending.bytes.len();
        if pending.bytes.len() - pending.start > MAX_STREAM_LINE_BYTES {
            return Err(line_too_long_error());
        }
        if pending.eof {
            if pending.start == pending.bytes.len() {
                return Ok(None);
            }
            let line = pending.bytes[pending.start..].to_vec();
            pending.start = pending.bytes.len();
            return Ok(Some(line));
        }

        // Avoid shifting the unread suffix for every short line. A buffer is
        // compacted only when another read is required; long lines are scanned
        // incrementally instead of rescanning their entire prefix each time.
        if pending.start > 0 {
            pending.bytes.drain(..pending.start);
            pending.scanned -= pending.start;
            pending.start = 0;
        }
        let mut buf = [0_u8; 8192];
        tokio::task::yield_now().await;
        let n = reader.read(&mut buf).await?;
        pending.eof = n == 0;
        pending.bytes.extend_from_slice(&buf[..n]);
    }
}

fn line_text_lossy(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let body = text.strip_suffix('\n').unwrap_or(&text);
    body.strip_suffix('\r').unwrap_or(body).to_owned()
}

async fn counts_reader(reader: &mut BoxAsyncRead) -> io::Result<Counts> {
    let mut counts = Counts::default();
    let mut in_word = false;
    let mut buf = vec![0; 64 * 1024];
    loop {
        tokio::task::yield_now().await;
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(counts);
        }
        counts.bytes += n;
        for &byte in &buf[..n] {
            if byte == b'\n' {
                counts.lines += 1;
            }
            // GNU wc in the C locale treats word separators as ASCII whitespace.
            if byte.is_ascii_whitespace() {
                in_word = false;
            } else if !in_word {
                counts.words += 1;
                in_word = true;
            }
        }
    }
}

async fn sed_reader(
    reader: &mut BoxAsyncRead,
    stdout: &mut BoxAsyncWrite,
    regex: &Regex,
    sub: &SedSubstitution,
) -> io::Result<()> {
    let mut pending = LineBuffer::default();
    while let Some(line) = read_line(reader, &mut pending).await? {
        let text = String::from_utf8_lossy(&line);
        let (body, ending) = text
            .strip_suffix('\n')
            .map_or((text.as_ref(), ""), |body| (body, "\n"));
        let mut copied = 0;
        for (index, captures) in regex.captures_iter(body).enumerate() {
            if index.is_multiple_of(1024) {
                tokio::task::yield_now().await;
            }
            let matched = captures.get(0).expect("whole match exists");
            write_sed_piece(stdout, &body.as_bytes()[copied..matched.start()]).await?;
            write_sed_replacement(stdout, &sub.replacement, &captures).await?;
            copied = matched.end();
            if !sub.global {
                break;
            }
        }
        write_sed_piece(stdout, &body.as_bytes()[copied..]).await?;
        stdout.write_all(ending.as_bytes()).await?;
    }
    Ok(())
}

async fn uniq_reader(
    reader: &mut BoxAsyncRead,
    stdout: &mut BoxAsyncWrite,
    count: bool,
    repeated: bool,
    unique_only: bool,
) -> io::Result<()> {
    let mut pending = LineBuffer::default();
    let mut current: Option<(String, usize)> = None;
    while let Some(line) = read_line(reader, &mut pending).await? {
        let line = line_text_lossy(&line);
        if let Some((last, n)) = &mut current
            && *last == line
        {
            *n += 1;
            continue;
        }
        if let Some((line, n)) = current.take() {
            write_uniq_line(stdout, &line, n, count, repeated, unique_only).await?;
        }
        current = Some((line, 1));
    }
    if let Some((line, n)) = current {
        write_uniq_line(stdout, &line, n, count, repeated, unique_only).await?;
    }
    Ok(())
}

async fn write_uniq_line(
    stdout: &mut BoxAsyncWrite,
    line: &str,
    n: usize,
    count: bool,
    repeated: bool,
    unique_only: bool,
) -> io::Result<()> {
    if repeated && n == 1 {
        return Ok(());
    }
    if unique_only && n != 1 {
        return Ok(());
    }
    if count {
        stdout
            .write_all(format!("{n:>7} {line}\n").as_bytes())
            .await
    } else {
        stdout.write_all(format!("{line}\n").as_bytes()).await
    }
}

async fn write_counts(
    stdout: &mut BoxAsyncWrite,
    counts: Counts,
    name: Option<&str>,
    show_lines: bool,
    show_words: bool,
    show_bytes: bool,
    width: usize,
) {
    let mut out = String::new();
    let mut wrote_count = false;
    if show_lines {
        if wrote_count {
            out.push(' ');
        }
        out.push_str(&format!("{:>width$}", counts.lines));
        wrote_count = true;
    }
    if show_words {
        if wrote_count {
            out.push(' ');
        }
        out.push_str(&format!("{:>width$}", counts.words));
        wrote_count = true;
    }
    if show_bytes {
        if wrote_count {
            out.push(' ');
        }
        out.push_str(&format!("{:>width$}", counts.bytes));
    }
    if let Some(name) = name {
        out.push(' ');
        out.push_str(name);
    }
    out.push('\n');
    let _ = stdout.write_all(out.as_bytes()).await;
}

fn numeric_key(line: &str) -> f64 {
    let trimmed = line.trim_start();
    let mut end = 0;
    let mut chars = trimmed.char_indices().peekable();
    if matches!(chars.peek(), Some((_, '+' | '-'))) {
        let (_, sign) = chars.next().expect("peeked sign");
        end = sign.len_utf8();
    }
    let mut digits = 0;
    while let Some((index, ch)) = chars.peek().copied() {
        if ch.is_ascii_digit() {
            digits += 1;
            end = index + ch.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    if matches!(chars.peek(), Some((_, '.'))) {
        let (_, dot) = chars.next().expect("peeked dot");
        end += dot.len_utf8();
        while let Some((index, ch)) = chars.peek().copied() {
            if ch.is_ascii_digit() {
                end = index + ch.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
    }
    if digits == 0 {
        0.0
    } else {
        trimmed[..end].parse::<f64>().unwrap_or(0.0)
    }
}

fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("/")
        .to_owned()
}

fn expand_echo_escapes(input: &str) -> (String, bool) {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('0') => {
                let mut value = 0_u32;
                let mut consumed = 0;
                while consumed < 3 {
                    let Some(next) = chars.peek().copied() else {
                        break;
                    };
                    let Some(digit) = next.to_digit(8) else {
                        break;
                    };
                    value = value * 8 + digit;
                    chars.next();
                    consumed += 1;
                }
                out.push(char::from_u32(value).unwrap_or('\0'));
            }
            Some('a') => out.push('\u{0007}'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{0008}'),
            Some('e') | Some('E') => out.push('\u{001b}'),
            Some('f') => out.push('\u{000c}'),
            Some('v') => out.push('\u{000b}'),
            Some('x') => {
                let mut value = 0_u32;
                let mut consumed = 0;
                while consumed < 2 {
                    let Some(next) = chars.peek().copied() else {
                        break;
                    };
                    let Some(digit) = next.to_digit(16) else {
                        break;
                    };
                    value = value * 16 + digit;
                    chars.next();
                    consumed += 1;
                }
                if consumed == 0 {
                    out.push('\\');
                    out.push('x');
                } else {
                    out.push(char::from_u32(value).unwrap_or('\0'));
                }
            }
            Some('u') => push_hex_escape(&mut out, &mut chars, 4, 'u'),
            Some('U') => push_hex_escape(&mut out, &mut chars, 8, 'U'),
            Some('\\') => out.push('\\'),
            Some('c') => return (out, true),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    (out, false)
}

fn push_hex_escape(
    out: &mut String,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    width: usize,
    marker: char,
) {
    let mut value = 0_u32;
    let mut consumed = 0;
    while consumed < width {
        let Some(next) = chars.peek().copied() else {
            break;
        };
        let Some(digit) = next.to_digit(16) else {
            break;
        };
        value = value * 16 + digit;
        chars.next();
        consumed += 1;
    }
    if consumed == 0 {
        out.push('\\');
        out.push(marker);
    } else if let Some(ch) = char::from_u32(value) {
        out.push(ch);
    }
}

fn parse_sed_substitution(script: &str) -> Option<SedSubstitution> {
    let mut chars = script.chars();
    if chars.next()? != 's' {
        return None;
    }
    let delimiter = chars.next()?;
    let rest: String = chars.collect();
    let parts = split_sed_parts(&rest, delimiter)?;
    let flags = parts.2;
    if flags.chars().any(|flag| flag != 'g' && flag != 'i') {
        return None;
    }
    Some(SedSubstitution {
        pattern: parts.0,
        replacement: parts.1,
        global: flags.contains('g'),
        ignore_case: flags.contains('i'),
    })
}

fn split_sed_parts(input: &str, delimiter: char) -> Option<(String, String, String)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if chars.peek() == Some(&delimiter) {
                current.push(delimiter);
                chars.next();
            } else {
                current.push('\\');
            }
            continue;
        }
        if ch == delimiter && parts.len() < 2 {
            parts.push(current);
            current = String::new();
        } else {
            current.push(ch);
        }
    }
    if parts.len() != 2 {
        return None;
    }
    Some((parts.remove(0), parts.remove(0), current))
}

// Stream replacement segments rather than allocating match_count × replacement
// bytes (or capture_length × ampersand_count) into one expanded line.
async fn write_sed_piece(stdout: &mut BoxAsyncWrite, bytes: &[u8]) -> io::Result<()> {
    for chunk in bytes.chunks(STREAM_CHUNK_BYTES) {
        tokio::task::yield_now().await;
        stdout.write_all(chunk).await?;
    }
    Ok(())
}

async fn write_sed_replacement(
    stdout: &mut BoxAsyncWrite,
    replacement: &str,
    captures: &Captures<'_>,
) -> io::Result<()> {
    let mut rest = replacement;
    let mut tokens = 0usize;
    while let Some(index) = rest.find(['&', '\\']) {
        if tokens.is_multiple_of(1024) {
            tokio::task::yield_now().await;
        }
        tokens += 1;
        write_sed_piece(stdout, &rest.as_bytes()[..index]).await?;
        rest = &rest[index..];
        if rest.starts_with('&') {
            if let Some(matched) = captures.get(0) {
                write_sed_piece(stdout, matched.as_str().as_bytes()).await?;
            }
            rest = &rest[1..];
            continue;
        }
        rest = &rest[1..];
        let Some(ch) = rest.chars().next() else {
            return stdout.write_all(b"\\").await;
        };
        rest = &rest[ch.len_utf8()..];
        match ch {
            'n' => stdout.write_all(b"\n").await?,
            't' => stdout.write_all(b"\t").await?,
            digit @ '1'..='9' => {
                let index = digit.to_digit(10).expect("decimal capture") as usize;
                if let Some(group) = captures.get(index) {
                    write_sed_piece(stdout, group.as_str().as_bytes()).await?;
                }
            }
            ch => {
                let mut encoded = [0u8; 4];
                stdout
                    .write_all(ch.encode_utf8(&mut encoded).as_bytes())
                    .await?;
            }
        }
    }
    write_sed_piece(stdout, rest.as_bytes()).await
}

fn validate_sed_replacement(replacement: &str, captures_len: usize) -> Result<(), usize> {
    let mut chars = replacement.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            continue;
        }
        let Some(digit @ '1'..='9') = chars.next() else {
            continue;
        };
        let index = digit.to_digit(10).expect("decimal capture") as usize;
        if index >= captures_len {
            return Err(index);
        }
    }
    Ok(())
}

fn parse_head_tail_count(value: &str, head_mode: bool) -> Result<TailCount, ()> {
    if let Some(from) = value.strip_prefix('+') {
        if head_mode {
            return Err(());
        }
        let from = from.parse::<usize>().map_err(|_| ())?;
        return Ok(TailCount::From(from.max(1)));
    }
    value.parse::<usize>().map(TailCount::Last).map_err(|_| ())
}

fn selected_counts(
    counts: Counts,
    show_lines: bool,
    show_words: bool,
    show_bytes: bool,
) -> Vec<usize> {
    let mut values = Vec::new();
    if show_lines {
        values.push(counts.lines);
    }
    if show_words {
        values.push(counts.words);
    }
    if show_bytes {
        values.push(counts.bytes);
    }
    values
}

fn decimal_width(value: usize) -> usize {
    value.to_string().len()
}

#[derive(Debug, Clone, Copy, Default)]
struct GrepFlags {
    ignore_case: bool,
    invert: bool,
    line_numbers: bool,
    count: bool,
    recursive: bool,
}

#[derive(Debug, Clone)]
struct SedSubstitution {
    pattern: String,
    replacement: String,
    global: bool,
    ignore_case: bool,
}

#[derive(Debug, Clone, Copy)]
struct HeadTailSettings {
    count: TailCount,
    head_mode: bool,
    window_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
enum TailCount {
    Last(usize),
    From(usize),
}

#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    lines: usize,
    words: usize,
    bytes: usize,
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, rhs: Self) {
        self.lines += rhs.lines;
        self.words += rhs.words;
        self.bytes += rhs.bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::basename;

    #[tokio::test]
    async fn jq_waiting_for_worker_admission_obeys_exec_deadline() {
        use crate::sandbox::{Limits, Sandbox};
        use std::sync::Arc;
        use std::time::Duration;

        let occupied = Arc::clone(&super::JQ_WORKERS)
            .acquire_many_owned(super::MAX_JQ_WORKERS as u32)
            .await
            .unwrap();
        let sandbox = Sandbox::builder()
            .limits(Limits {
                wall_time: Duration::from_millis(20),
                ..Limits::default()
            })
            .build();
        let result = sandbox.exec("jq -n '1'").await;
        assert_eq!(
            result.exit_code, 124,
            "a full worker pool must defer evaluation"
        );
        drop(occupied);
        let result = Sandbox::builder().build().exec("jq -n '1'").await;
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout, "1\n");

        // Leave one worker slot for a two-jq pipeline whose output exceeds
        // both the pipe and message-channel buffers. The downstream reader
        // must drain input before waiting for its own evaluator slot.
        let occupied = Arc::clone(&super::JQ_WORKERS)
            .acquire_many_owned((super::MAX_JQ_WORKERS - 1) as u32)
            .await
            .unwrap();
        let sandbox = Sandbox::builder()
            .limits(Limits {
                wall_time: Duration::from_secs(2),
                ..Limits::default()
            })
            .build();
        let result = sandbox
            .exec("jq -nc '\"x\" * 1000000' | jq -c . | wc -c")
            .await;
        assert_eq!(
            result.exit_code, 0,
            "queued jq must drain its input: {}",
            result.stderr
        );
        assert_eq!(result.stdout.trim(), "1000003");
        drop(occupied);
    }

    #[test]
    fn basename_handles_root_and_trailing_slashes() {
        // Root-like paths have no non-empty path component; callers use "/"
        // as the GNU-shaped display/name fallback.
        assert_eq!(basename("/"), "/");
        assert_eq!(basename("////"), "/");
        assert_eq!(basename("/tmp/file///"), "file");
    }
}
