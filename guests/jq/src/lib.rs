//! Jq compiler/evaluator contained in a fresh Wasm instance per command.
//! Imports move bounded byte chunks and read the host clock; no WASI is linked.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
use std::io;

mod evaluator;
#[allow(dead_code)]
#[path = "../../../src/sandbox/jq_protocol.rs"]
mod protocol;
use evaluator::{self as jq, JqError};
use protocol::{JqInputSource, JqOptions};

const MAX_JQ_JSON_NESTING: usize = 1024;
struct JqRunDone {
    exit_code: i32,
    stderr: Vec<u8>,
}

fn parse_variables(options: &JqOptions) -> Result<Vec<jaq_json::Val>, String> {
    options
        .vars
        .iter()
        .map(|var| {
            if var.json {
                let name = var.name.trim_start_matches('$');
                validate_jq_json_nesting(var.value.as_bytes())
                    .map_err(|err| format!("jq: invalid JSON for --argjson {name}: {err}\n"))?;
                jaq_json::read::parse_single(var.value.as_bytes())
                    .map_err(|err| format!("jq: invalid JSON for --argjson {name}: {err}\n"))
            } else {
                Ok(jaq_json::Val::utf8_str(var.value.clone()))
            }
        })
        .collect()
}

fn is_broken_pipe(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::BrokenPipe
}

fn validate_jq_json_nesting(data: &[u8]) -> Result<(), String> {
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
                    return Err(format!(
                        "JSON nesting exceeds maximum depth {MAX_JQ_JSON_NESTING}"
                    ));
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    Ok(())
}

fn run_program(
    options: &JqOptions,
    inputs: Vec<JqInputSource>,
    out: &mut impl io::Write,
) -> JqRunDone {
    let vars = match parse_variables(options) {
        Ok(vars) => vars,
        Err(message) => {
            return JqRunDone {
                exit_code: 2,
                stderr: message.into_bytes(),
            };
        }
    };
    let global_vars: Vec<_> = options.vars.iter().map(|var| var.name.clone()).collect();
    let program = match jq::compile_with_vars(&options.filter, &global_vars) {
        Ok(program) => program,
        Err(err) => return jq_error_outcome(err),
    };

    let mut last_output = None;

    if options.null_input {
        if let Err(done) = run_jq_input_value(
            &program,
            jaq_json::Val::Null,
            &vars,
            options,
            out,
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
                out,
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
                        run_jq_input_value(&program, value, &vars, options, out, &mut last_output)
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
    out: &mut impl io::Write,
    last_output: &mut Option<bool>,
) -> Result<(), JqRunDone> {
    for value in program.output_iter(input, vars) {
        let value = value.map_err(jq_error_outcome)?;
        *last_output = Some(jq_truthy(&value));
        write_jq_value(out, &value, options)
            .and_then(|()| io::Write::flush(out))
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

#[cfg(target_arch = "wasm32")]
mod abi {
    use super::{JqInputSource, JqRunDone, protocol::JqRequest, run_program};
    use std::io::{self, BufWriter, Write};

    const CHUNK: usize = 64 * 1024;

    #[link(wasm_import_module = "tinysandbox_jq")]
    unsafe extern "C" {
        fn input_len(index: i32) -> i32;
        fn read_input(index: i32, offset: i32, ptr: *mut u8, len: i32) -> i32;
        fn write_output(kind: i32, ptr: *const u8, len: i32) -> i32;
        pub(super) fn now() -> f64;
    }

    fn input(index: i32) -> io::Result<Vec<u8>> {
        let len = unsafe { input_len(index) };
        let len = usize::try_from(len).map_err(|_| io::Error::other("invalid input length"))?;
        let mut bytes = vec![0; len];
        let mut offset = 0;
        while offset < len {
            let want = (len - offset).min(CHUNK);
            let n = unsafe {
                read_input(
                    index,
                    offset as i32,
                    bytes[offset..].as_mut_ptr(),
                    want as i32,
                )
            };
            let n = usize::try_from(n).map_err(|_| io::Error::other("input unavailable"))?;
            if n == 0 || n > want {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short host input",
                ));
            }
            offset += n;
        }
        Ok(bytes)
    }

    struct Output(i32);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.is_empty() {
                return Ok(0);
            }
            let want = bytes.len().min(CHUNK);
            let n = unsafe { write_output(self.0, bytes.as_ptr(), want as i32) };
            let n = usize::try_from(n)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "output closed"))?;
            if n == 0 || n > want {
                return Err(io::Error::other("invalid host write"));
            }
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn execute() -> JqRunDone {
        let result = (|| -> Result<_, String> {
            let config = input(0).map_err(|err| format!("jq: input error: {err}\n"))?;
            let request: JqRequest = serde_json::from_slice(&config)
                .map_err(|err| format!("jq: configuration error: {err}\n"))?;
            drop(config);
            let mut inputs = Vec::new();
            for (index, path) in request.paths.into_iter().enumerate() {
                let data = input((index + 1) as i32)
                    .map_err(|err| format!("jq: {path}: input error: {err}\n"))?;
                inputs.push(JqInputSource { path, data });
            }
            Ok((request.options, inputs))
        })();
        match result {
            Ok((options, inputs)) => {
                let mut stdout = BufWriter::with_capacity(CHUNK, Output(1));
                run_program(&options, inputs, &mut stdout)
            }
            Err(message) => JqRunDone {
                exit_code: 2,
                stderr: message.into_bytes(),
            },
        }
    }

    /// Each call runs in a fresh bounded instance; no allocator exports or WASI
    /// state are needed by the host.
    #[unsafe(no_mangle)]
    pub extern "C" fn run() -> i32 {
        let result = execute();
        let _ = Output(2).write_all(&result.stderr);
        result.exit_code
    }
}

fn host_now() -> f64 {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        abi::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::{JqInputSource, protocol::parse_jq_args, run_program};

    fn run(args: &[&str], data: &[u8]) -> (i32, String, String) {
        let options = parse_jq_args(args.iter().map(|arg| (*arg).to_owned()).collect()).unwrap();
        let inputs = vec![JqInputSource {
            path: "-".to_owned(),
            data: data.to_vec(),
        }];
        let mut output = Vec::new();
        let done = run_program(&options, inputs, &mut output);
        (
            done.exit_code,
            String::from_utf8(output).unwrap(),
            String::from_utf8(done.stderr).unwrap(),
        )
    }

    #[test]
    fn cli_variables_are_parsed_inside_guest_and_errors_keep_exit_two() {
        assert_eq!(
            run(
                &[
                    "-nc",
                    "--arg",
                    "name",
                    "Ada",
                    "--argjson",
                    "count",
                    "2",
                    "{name:$name,count:$count}"
                ],
                b""
            )
            .1,
            "{\"name\":\"Ada\",\"count\":2}\n"
        );
        let (code, output, error) = run(&["-n", "--argjson", "x", "[", "."], b"");
        assert_eq!(code, 2);
        assert!(output.is_empty());
        assert!(error.contains("invalid JSON for --argjson x"));
    }

    #[test]
    fn utc_date_functions_and_now_are_supported_without_wasi() {
        assert_eq!(
            run(&["-nr", "0 | strflocaltime(\"%Y-%m-%d %H:%M:%S %z\")"], b"").1,
            "1970-01-01 00:00:00 +0000\n"
        );
        assert_eq!(
            run(&["-nc", "0 | localtime"], b"").1,
            "[1970,0,1,0,0,0,4,0]\n"
        );
        assert_eq!(
            run(
                &[
                    "-nr",
                    "\"2000-01-02T03:04:05Z\" | fromdateiso8601 | todateiso8601"
                ],
                b""
            )
            .1,
            "2000-01-02T03:04:05Z\n"
        );
        let (code, output, error) = run(&["-n", "now > 0"], b"");
        assert_eq!((code, output.as_str(), error.as_str()), (0, "true\n", ""));
    }
}
