//! Native command implementations.
//!
//! `grep` and `sed s///` deliberately use Rust's regular-expression dialect
//! instead of GNU BRE/ERE syntax so matching remains linear-time. Unsupported
//! GNU regex syntax therefore fails at compile time instead of being
//! interpreted differently.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use regex::{Captures, Regex, RegexBuilder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::sandbox::command::{
    BoxAsyncWrite, Command, CommandContext, CommandFuture, CommandResult,
};
use crate::sandbox::fs::{Fs, errno_message, join_path};
use crate::vfs::{Errno, FileType, Metadata, VfsError};

pub(crate) fn register(commands: &mut BTreeMap<String, Arc<dyn Command>>) {
    insert(commands, "cat", cat);
    insert(commands, "cp", cp);
    insert(commands, "echo", echo);
    insert(commands, "false", false_cmd);
    insert(commands, "grep", grep);
    insert(commands, "head", head);
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
            let data = if path == "-" {
                let mut data = Vec::new();
                if stdin.read_to_end(&mut data).await.is_err() {
                    return CommandResult::failure();
                }
                Ok(data)
            } else {
                fs.read_file(&path).await
            };
            match data {
                Ok(data) => {
                    let output = if flags.plain() {
                        data
                    } else {
                        let mut out = Vec::with_capacity(data.len());
                        cat_transform(&data, &flags, &mut state, &mut out);
                        out
                    };
                    if stdout.write_all(&output).await.is_err() {
                        return CommandResult::failure();
                    }
                }
                Err(err) => {
                    exit = 1;
                    write_vfs_error(&mut stderr, "cat", &path, err).await;
                }
            }
        }
        CommandResult::new(exit)
    })
}

fn echo(ctx: CommandContext) -> CommandFuture {
    boxed(async move {
        let CommandContext {
            args,
            mut stdout,
            stderr: _,
            ..
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
        let (files, mut had_error) =
            collect_input_files(&fs, &positional, flags.recursive, "grep", &mut stderr).await;
        let show_path = flags.recursive || files.len() > 1 || positional.len() > 1;
        let mut matched_any = false;
        if positional.is_empty() {
            let mut data = Vec::new();
            let _ = stdin.read_to_end(&mut data).await;
            matched_any |= grep_bytes(&mut stdout, "", &data, &regex, flags, false).await;
        } else {
            for path in files {
                let data = if path == "-" {
                    let mut data = Vec::new();
                    match stdin.read_to_end(&mut data).await {
                        Ok(_) => Ok(data),
                        Err(_) => Err(VfsError::new(Errno::EINVAL)),
                    }
                } else {
                    fs.read_file(&path).await
                };
                match data {
                    Ok(data) => {
                        let label = if path == "-" {
                            "(standard input)"
                        } else {
                            &path
                        };
                        matched_any |=
                            grep_bytes(&mut stdout, label, &data, &regex, flags, show_path).await;
                    }
                    Err(err) => {
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
        let input = match read_inputs(&fs, &files, &mut stdin, "sort", &mut stderr).await {
            Ok(input) => input,
            Err(()) => return CommandResult::new(2),
        };
        if input.len() > limits.sort_input_bytes {
            let _ = stderr
                .write_all(b"sort: input too large for tinysandbox sort\n")
                .await;
            return CommandResult::new(2);
        }
        let mut lines = text_lines_lossy(&input);
        if numeric {
            lines.sort_by(|a, b| {
                numeric_key(a)
                    .total_cmp(&numeric_key(b))
                    .then_with(|| a.cmp(b))
            });
        } else {
            lines.sort();
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
        for line in lines {
            let _ = stdout.write_all(line.as_bytes()).await;
            let _ = stdout.write_all(b"\n").await;
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
        let input = match read_inputs(&fs, &files, &mut stdin, "uniq", &mut stderr).await {
            Ok(input) => input,
            Err(()) => return CommandResult::new(1),
        };
        for (line, n) in adjacent_counts(text_lines_lossy(&input)) {
            if repeated && n == 1 {
                continue;
            }
            if unique_only && n != 1 {
                continue;
            }
            if count {
                let _ = stdout
                    .write_all(format!("{n:>7} {line}\n").as_bytes())
                    .await;
            } else {
                let _ = stdout.write_all(format!("{line}\n").as_bytes()).await;
            }
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
            let mut input = Vec::new();
            let _ = stdin.read_to_end(&mut input).await;
            let counts = counts_for(&input);
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
                let data = if path == "-" {
                    let mut data = Vec::new();
                    match stdin.read_to_end(&mut data).await {
                        Ok(_) => Ok(data),
                        Err(_) => Err(VfsError::new(Errno::EINVAL)),
                    }
                } else {
                    fs.read_file(path).await
                };
                match data {
                    Ok(data) => {
                        let counts = counts_for(&data);
                        total += counts;
                        rows.push((counts, path.as_str()));
                    }
                    Err(err) => {
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
                .write_all(b"sed: unsupported command; tinysandbox supports s/// with g and i flags\n")
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
        let input = match read_inputs(&fs, &files, &mut stdin, "sed", &mut stderr).await {
            Ok(input) => input,
            Err(()) => return CommandResult::new(2),
        };
        let out = apply_sed_substitution(&input, &regex, &sub);
        let _ = stdout.write_all(out.as_bytes()).await;
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
    let inputs = if files.is_empty() {
        let mut input = Vec::new();
        if stdin.read_to_end(&mut input).await.is_err() {
            return CommandResult::new(1);
        }
        vec![("standard input".to_owned(), input)]
    } else {
        let mut inputs = Vec::new();
        for file in &files {
            let data = if file == "-" {
                let mut data = Vec::new();
                match stdin.read_to_end(&mut data).await {
                    Ok(_) => Ok(data),
                    Err(_) => Err(VfsError::new(Errno::EINVAL)),
                }
            } else {
                fs.read_file(file).await
            };
            match data {
                Ok(data) => inputs.push((file.clone(), data)),
                Err(err) => {
                    write_vfs_error(&mut stderr, cmd, file, err).await;
                    return CommandResult::new(1);
                }
            }
        }
        inputs
    };
    let show_headers = verbose || inputs.len() > 1;
    for (index, (label, input)) in inputs.iter().enumerate() {
        if show_headers {
            if index > 0 {
                let _ = stdout.write_all(b"\n").await;
            }
            let _ = stdout
                .write_all(format!("==> {label} <==\n").as_bytes())
                .await;
        }
        let mut lines = lines_with_endings(input);
        match n {
            TailCount::Last(count) if head_mode => lines.truncate(count),
            TailCount::Last(count) if lines.len() > count => {
                lines = lines[lines.len() - count..].to_vec();
            }
            TailCount::From(start) => {
                let skip = start.saturating_sub(1);
                lines = if skip < lines.len() {
                    lines[skip..].to_vec()
                } else {
                    Vec::new()
                };
            }
            TailCount::Last(_) => {}
        }
        for line in lines {
            let _ = stdout.write_all(&line).await;
        }
    }
    CommandResult::success()
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
    let mut current = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        current.push('/');
        current.push_str(part);
        match fs.mkdir(&current).await {
            Ok(()) => {}
            Err(err) if err.errno() == Errno::EEXIST => {}
            Err(err) => return Err(err),
        }
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
    let metadata = fs.stat(source).await?;
    if metadata.file_type == FileType::Directory {
        if !recursive {
            return Err(VfsError::new(Errno::EISDIR));
        }
        match fs.mkdir(dest).await {
            Ok(()) => {}
            Err(err) if err.errno() == Errno::EEXIST => {}
            Err(err) => return Err(err),
        }
        for entry in fs.readdir(source).await? {
            let child_source = join_path(source, &entry.name);
            let child_dest = join_path(dest, &entry.name);
            Box::pin(copy_path(fs, &child_source, &child_dest, recursive)).await?;
        }
    } else {
        let data = fs.read_file(source).await?;
        fs.write_file(dest, &data, false).await?;
    }
    Ok(())
}

async fn remove_path(fs: &Fs, path: &str, recursive: bool) -> Result<(), VfsError> {
    let metadata = fs.stat(path).await?;
    if metadata.file_type == FileType::Directory {
        if !recursive {
            return Err(VfsError::new(Errno::EISDIR));
        }
        for entry in fs.readdir(path).await? {
            let child = join_path(path, &entry.name);
            Box::pin(remove_path(fs, &child, recursive)).await?;
        }
        fs.rmdir(path).await
    } else {
        fs.unlink(path).await
    }
}

async fn collect_input_files(
    fs: &Fs,
    paths: &[String],
    recursive: bool,
    cmd: &str,
    stderr: &mut BoxAsyncWrite,
) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut had_error = false;
    for path in paths {
        if path == "-" {
            out.push(path.clone());
            continue;
        }
        match fs.stat(path).await {
            Ok(meta) if meta.file_type == FileType::Directory && recursive => {
                collect_files_recursive(fs, path, &mut out).await;
            }
            Ok(_) => out.push(path.clone()),
            Err(err) => {
                had_error = true;
                write_vfs_error(stderr, cmd, path, err).await;
            }
        }
    }
    (out, had_error)
}

async fn collect_files_recursive(fs: &Fs, path: &str, out: &mut Vec<String>) {
    let Ok(entries) = fs.readdir(path).await else {
        return;
    };
    for entry in entries {
        let child = join_path(path, &entry.name);
        if entry.metadata.file_type == FileType::Directory {
            Box::pin(collect_files_recursive(fs, &child, out)).await;
        } else {
            out.push(child);
        }
    }
}

async fn grep_bytes(
    stdout: &mut BoxAsyncWrite,
    path: &str,
    data: &[u8],
    regex: &Regex,
    flags: GrepFlags,
    show_path: bool,
) -> bool {
    let text = String::from_utf8_lossy(data);
    let mut matched = 0_usize;
    for (index, line) in text.lines().enumerate() {
        let is_match = regex.is_match(line) ^ flags.invert;
        if !is_match {
            continue;
        }
        matched += 1;
        if flags.count {
            continue;
        }
        if show_path {
            let _ = stdout.write_all(format!("{path}:").as_bytes()).await;
        }
        if flags.line_numbers {
            let _ = stdout.write_all(format!("{}:", index + 1).as_bytes()).await;
        }
        let _ = stdout.write_all(format!("{line}\n").as_bytes()).await;
    }
    if flags.count {
        if show_path {
            let _ = stdout.write_all(format!("{path}:").as_bytes()).await;
        }
        let _ = stdout.write_all(format!("{matched}\n").as_bytes()).await;
    }
    matched > 0
}

async fn read_inputs(
    fs: &Fs,
    files: &[String],
    stdin: &mut Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    cmd: &str,
    stderr: &mut BoxAsyncWrite,
) -> Result<Vec<u8>, ()> {
    let mut input = Vec::new();
    if files.is_empty() {
        stdin.read_to_end(&mut input).await.map_err(|_| ())?;
        return Ok(input);
    }
    for file in files {
        let data = if file == "-" {
            let mut data = Vec::new();
            match stdin.read_to_end(&mut data).await {
                Ok(_) => Ok(data),
                Err(_) => Err(VfsError::new(Errno::EINVAL)),
            }
        } else {
            fs.read_file(file).await
        };
        match data {
            Ok(mut data) => input.append(&mut data),
            Err(err) => {
                write_vfs_error(stderr, cmd, file, err).await;
                return Err(());
            }
        }
    }
    Ok(input)
}

fn lines_with_endings(input: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in input.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(input[start..=index].to_vec());
            start = index + 1;
        }
    }
    if start < input.len() {
        lines.push(input[start..].to_vec());
    }
    lines
}

fn text_lines_lossy(input: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(input)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn adjacent_counts(lines: Vec<String>) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for line in lines {
        if let Some((last, n)) = out.last_mut()
            && *last == line
        {
            *n += 1;
            continue;
        }
        out.push((line, 1));
    }
    out
}

fn counts_for(input: &[u8]) -> Counts {
    Counts {
        lines: input.iter().filter(|byte| **byte == b'\n').count(),
        words: String::from_utf8_lossy(input).split_whitespace().count(),
        bytes: input.len(),
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

fn apply_sed_substitution(input: &[u8], regex: &Regex, sub: &SedSubstitution) -> String {
    let mut out = String::new();
    for line in lines_with_endings(input) {
        let text = String::from_utf8_lossy(&line);
        let (body, ending) = text
            .strip_suffix('\n')
            .map_or((text.as_ref(), ""), |body| (body, "\n"));
        if sub.global {
            let replaced = regex.replace_all(body, |captures: &Captures<'_>| {
                expand_sed_replacement(&sub.replacement, captures)
            });
            out.push_str(&replaced);
        } else {
            let replaced = regex.replace(body, |captures: &Captures<'_>| {
                expand_sed_replacement(&sub.replacement, captures)
            });
            out.push_str(&replaced);
        }
        out.push_str(ending);
    }
    out
}

fn expand_sed_replacement(replacement: &str, captures: &Captures<'_>) -> String {
    let mut out = String::new();
    let mut chars = replacement.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '&' => {
                if let Some(matched) = captures.get(0) {
                    out.push_str(matched.as_str());
                }
            }
            '\\' => match chars.next() {
                Some('&') => out.push('&'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(digit @ '1'..='9') => {
                    let index = digit.to_digit(10).expect("decimal capture") as usize;
                    if let Some(group) = captures.get(index) {
                        out.push_str(group.as_str());
                    }
                }
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            _ => out.push(ch),
        }
    }
    out
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
