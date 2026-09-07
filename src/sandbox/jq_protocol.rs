//! Shared, data-only host/guest jq configuration and CLI parsing.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JqOptions {
    pub filter: String,
    pub files: Vec<String>,
    pub raw_output: bool,
    pub join_output: bool,
    pub compact_output: bool,
    pub exit_status: bool,
    pub null_input: bool,
    pub slurp: bool,
    pub sort_keys: bool,
    pub indent: String,
    pub vars: Vec<JqVariable>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JqVariable {
    pub name: String,
    pub value: String,
    pub json: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct JqInputSource {
    pub path: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct JqRequest {
    pub options: JqOptions,
    pub paths: Vec<String>,
}

pub(crate) fn parse_jq_args(args: Vec<String>) -> Result<JqOptions, String> {
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
                    value: value.to_owned(),
                    json: false,
                });
                i += 3;
            }
            "--argjson" => {
                let (name, value) = parse_jq_arg_pair(&args, i, "--argjson")?;
                vars.push(JqVariable {
                    name: format!("${name}"),
                    value: value.to_owned(),
                    json: true,
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
