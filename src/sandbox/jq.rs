//! Internal jq filter engine backed by jaq.

use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use jaq_core::load::lex::StrPart;
use jaq_core::load::parse::{BinaryOp, Pattern, Term};
use jaq_core::load::{Arena, File, Loader};
use jaq_core::path::{Part, Path};
use jaq_core::{Ctx, DataT, Exn, ValT, ValX, ValXs, Vars, data};
use jaq_json::Val as JaqValue;

type JaqData = data::JustLut<JaqValue>;
type JaqFilter = jaq_core::Filter<JaqData>;

const MAX_JQ_FILTER_SOURCE_BYTES: usize = 256 * 1024;
const MAX_JQ_FILTER_NESTING: usize = 512;
const MAX_JQ_FILTER_SYNTAX_TOKENS: usize = 1024;

thread_local! {
    static CONTROL: RefCell<Option<JqControl>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct JqControl {
    deadline: Option<Instant>,
    cancelled: Arc<AtomicBool>,
}

/// Compiled jq filter ready to evaluate against JSON input.
pub(crate) struct JqProgram {
    filter: JaqFilter,
}

/// Error returned while compiling or evaluating a jq filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JqError {
    /// The filter failed to parse or compile.
    Compile(String),
    /// The filter failed while evaluating input.
    Runtime(String),
    /// The filter halted explicitly.
    Halt(i32),
}

impl fmt::Display for JqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compile(message) => write!(f, "compile error: {message}"),
            Self::Runtime(message) => write!(f, "runtime error: {message}"),
            Self::Halt(code) => write!(f, "halted with exit code {code}"),
        }
    }
}

impl std::error::Error for JqError {}

/// Guard that restores the previous jq deadline when dropped.
pub(crate) struct JqDeadlineGuard {
    previous: Option<JqControl>,
}

impl Drop for JqDeadlineGuard {
    fn drop(&mut self) {
        CONTROL.replace(self.previous.take());
    }
}

/// Installs thread-local cancellation checked by tinysandbox jq native wrappers.
pub(crate) fn set_control(
    deadline: Option<Instant>,
    cancelled: Arc<AtomicBool>,
) -> JqDeadlineGuard {
    let previous = CONTROL.replace(Some(JqControl {
        deadline,
        cancelled,
    }));
    JqDeadlineGuard { previous }
}

/// Compiles a jq filter with the given jq global variable names.
pub(crate) fn compile_with_vars(
    filter: &str,
    global_vars: &[String],
) -> Result<JqProgram, JqError> {
    validate_filter_policy(filter)?;

    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(
            &arena,
            File {
                code: filter,
                path: (),
            },
        )
        .map_err(|errs| JqError::Compile(format!("{errs:?}")))?;

    let funs = tinysandbox_funs()
        .into_iter()
        .chain(jaq_core::funs::<JaqData>())
        .chain(jaq_std::funs::<JaqData>())
        .chain(jaq_json::funs::<JaqData>());
    let global_vars = global_vars.iter().map(String::as_str);
    let filter = jaq_core::Compiler::default()
        .with_funs(funs)
        .with_global_vars(global_vars)
        .compile(modules)
        .map_err(|errs| JqError::Compile(format!("{errs:?}")))?;

    Ok(JqProgram { filter })
}

fn validate_filter_policy(filter: &str) -> Result<(), JqError> {
    validate_filter_source(filter)?;

    let Some(term) = jaq_core::load::parse(filter, |parser| parser.term()) else {
        return Ok(());
    };
    if term_contains_user_def(&term) {
        return Err(JqError::Compile(
            "user-defined jq functions are not supported in tinysandbox".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterDelimiter {
    Paren,
    Bracket,
    Brace,
    Interpolation,
}

// jaq's parser recurses on prefix terms and operator chains as well as grouped
// delimiters, so the preflight bounds total significant syntax before parsing.
fn validate_filter_source(filter: &str) -> Result<(), JqError> {
    if filter.len() > MAX_JQ_FILTER_SOURCE_BYTES {
        return Err(JqError::Compile(format!(
            "jq filter source exceeds maximum size {MAX_JQ_FILTER_SOURCE_BYTES} bytes"
        )));
    }

    let mut stack = Vec::new();
    let mut syntax_tokens = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut in_comment = false;
    let bytes = filter.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let byte = bytes[i];

        if in_comment {
            if byte == b'\n' {
                in_comment = false;
            }
            i += 1;
            continue;
        }

        if in_string {
            if escaped {
                escaped = false;
                if byte == b'(' {
                    push_filter_delimiter(&mut stack, FilterDelimiter::Interpolation)?;
                    bump_filter_syntax_token(&mut syntax_tokens)?;
                    in_string = false;
                }
                i += 1;
                continue;
            }

            match byte {
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }

        match byte {
            byte if byte.is_ascii_whitespace() => i += 1,
            b'#' => {
                in_comment = true;
                i += 1;
            }
            b'"' => {
                bump_filter_syntax_token(&mut syntax_tokens)?;
                in_string = true;
                i += 1;
            }
            b'(' => {
                push_filter_delimiter(&mut stack, FilterDelimiter::Paren)?;
                bump_filter_syntax_token(&mut syntax_tokens)?;
                i += 1;
            }
            b'[' => {
                push_filter_delimiter(&mut stack, FilterDelimiter::Bracket)?;
                bump_filter_syntax_token(&mut syntax_tokens)?;
                i += 1;
            }
            b'{' => {
                push_filter_delimiter(&mut stack, FilterDelimiter::Brace)?;
                bump_filter_syntax_token(&mut syntax_tokens)?;
                i += 1;
            }
            b')' => {
                bump_filter_syntax_token(&mut syntax_tokens)?;
                if pop_filter_delimiter(&mut stack, FilterDelimiter::Paren) {
                    in_string = true;
                }
                i += 1;
            }
            b']' => {
                bump_filter_syntax_token(&mut syntax_tokens)?;
                let _ = pop_filter_delimiter(&mut stack, FilterDelimiter::Bracket);
                i += 1;
            }
            b'}' => {
                bump_filter_syntax_token(&mut syntax_tokens)?;
                let _ = pop_filter_delimiter(&mut stack, FilterDelimiter::Brace);
                i += 1;
            }
            byte if is_jq_ident_start(byte) => {
                bump_filter_syntax_token(&mut syntax_tokens)?;
                i += 1;
                while i < bytes.len() && is_jq_ident_continue(bytes[i]) {
                    i += 1;
                }
            }
            byte if byte.is_ascii_digit() => {
                bump_filter_syntax_token(&mut syntax_tokens)?;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            _ => {
                bump_filter_syntax_token(&mut syntax_tokens)?;
                i += jq_operator_len(&bytes[i..]);
            }
        }
    }

    Ok(())
}

fn push_filter_delimiter(
    stack: &mut Vec<FilterDelimiter>,
    delimiter: FilterDelimiter,
) -> Result<(), JqError> {
    stack.push(delimiter);
    if stack.len() > MAX_JQ_FILTER_NESTING {
        return Err(JqError::Compile(format!(
            "jq filter nesting exceeds maximum depth {MAX_JQ_FILTER_NESTING}"
        )));
    }
    Ok(())
}

fn pop_filter_delimiter(stack: &mut Vec<FilterDelimiter>, fallback: FilterDelimiter) -> bool {
    let popped = stack.pop().unwrap_or(fallback);
    popped == FilterDelimiter::Interpolation
}

fn bump_filter_syntax_token(tokens: &mut usize) -> Result<(), JqError> {
    *tokens += 1;
    if *tokens > MAX_JQ_FILTER_SYNTAX_TOKENS {
        return Err(JqError::Compile(format!(
            "jq filter complexity exceeds maximum token count {MAX_JQ_FILTER_SYNTAX_TOKENS}"
        )));
    }
    Ok(())
}

fn is_jq_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_jq_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn jq_operator_len(rest: &[u8]) -> usize {
    if matches!(
        rest,
        [b'/', b'/', ..]
            | [b'=', b'=', ..]
            | [b'!', b'=', ..]
            | [b'<', b'=', ..]
            | [b'>', b'=', ..]
            | [b'+', b'=', ..]
            | [b'-', b'=', ..]
            | [b'*', b'=', ..]
            | [b'/', b'=', ..]
            | [b'%', b'=', ..]
            | [b'|', b'=', ..]
    ) {
        2
    } else {
        1
    }
}

fn term_contains_user_def(term: &Term<&str>) -> bool {
    match term {
        Term::Id | Term::Recurse | Term::Num(_) | Term::Break(_) | Term::Var(_) => false,
        Term::Str(_, parts) => parts.iter().any(|part| match part {
            StrPart::Term(term) => term_contains_user_def(term),
            StrPart::Str(_) | StrPart::Char(_) => false,
        }),
        Term::Arr(term) => term.as_deref().is_some_and(term_contains_user_def),
        Term::Obj(entries) => entries.iter().any(|(key, value)| {
            term_contains_user_def(key) || value.as_ref().is_some_and(term_contains_user_def)
        }),
        Term::Neg(term) | Term::Label(_, term) | Term::TryCatch(term, None) => {
            term_contains_user_def(term)
        }
        Term::BinOp(left, op, right) => {
            term_contains_user_def(left)
                || binary_op_contains_user_def(op)
                || term_contains_user_def(right)
        }
        Term::Fold(_, input, pattern, args) => {
            term_contains_user_def(input)
                || pattern_contains_user_def(pattern)
                || args.iter().any(term_contains_user_def)
        }
        Term::TryCatch(term, Some(catch)) => {
            term_contains_user_def(term) || term_contains_user_def(catch)
        }
        Term::IfThenElse(branches, fallback) => {
            branches.iter().any(|(condition, body)| {
                term_contains_user_def(condition) || term_contains_user_def(body)
            }) || fallback.as_deref().is_some_and(term_contains_user_def)
        }
        Term::Def(_, _) => true,
        Term::Call(_, args) => args.iter().any(term_contains_user_def),
        Term::Path(head, path) => term_contains_user_def(head) || path_contains_user_def(path),
    }
}

fn binary_op_contains_user_def(op: &BinaryOp<&str>) -> bool {
    match op {
        BinaryOp::Pipe(Some(pattern)) => pattern_contains_user_def(pattern),
        BinaryOp::Pipe(None)
        | BinaryOp::Comma
        | BinaryOp::Alt
        | BinaryOp::Or
        | BinaryOp::And
        | BinaryOp::Math(_)
        | BinaryOp::Cmp(_)
        | BinaryOp::Assign
        | BinaryOp::Update
        | BinaryOp::UpdateMath(_)
        | BinaryOp::UpdateAlt => false,
    }
}

fn pattern_contains_user_def(pattern: &Pattern<&str>) -> bool {
    match pattern {
        Pattern::Var(_) => false,
        Pattern::Arr(patterns) => patterns.iter().any(pattern_contains_user_def),
        Pattern::Obj(entries) => entries.iter().any(|(key, pattern)| {
            term_contains_user_def(key) || pattern_contains_user_def(pattern)
        }),
    }
}

fn path_contains_user_def(path: &Path<Term<&str>>) -> bool {
    path.0.iter().any(|(part, _)| match part {
        Part::Index(term) => term_contains_user_def(term),
        Part::Range(from, to) => {
            from.as_ref().is_some_and(term_contains_user_def)
                || to.as_ref().is_some_and(term_contains_user_def)
        }
    })
}

impl JqProgram {
    /// Returns an iterator over outputs for one input value.
    pub(crate) fn output_iter<'a>(
        &'a self,
        input: JaqValue,
        vars: &'a [JaqValue],
    ) -> impl Iterator<Item = Result<JaqValue, JqError>> + 'a {
        let ctx = Ctx::<JaqData>::new(&self.filter.lut, Vars::new(vars.iter().cloned()));
        let mut iter = self.filter.id.run((ctx, input));
        std::iter::from_fn(move || {
            if let Err(err) = check_deadline() {
                return Some(Err(err));
            }
            iter.next()
                .map(|result| result.map_err(jq_exception_to_error))
        })
    }

    /// Evaluates the compiled filter against one jaq JSON input value.
    #[cfg(test)]
    pub(crate) fn evaluate_values(
        &self,
        input: JaqValue,
        vars: &[JaqValue],
    ) -> Result<Vec<JaqValue>, JqError> {
        self.output_iter(input, vars).collect()
    }
}

fn tinysandbox_funs() -> [jaq_core::native::Fun<JaqData>; 1] {
    let range: jaq_core::RunPtr<JaqData> = tinysandbox_range;
    [(
        "range",
        jaq_core::native::v(3),
        jaq_core::Native::<JaqData>::new(range),
    )]
}

fn tinysandbox_range<'a>(
    mut cv: jaq_core::Cv<'a, JaqData>,
) -> ValXs<'a, <JaqData as DataT>::V<'a>> {
    let by = cv.0.pop_var();
    let to = cv.0.pop_var();
    let from = cv.0.pop_var();
    Box::new(deadline_checked_range(Ok(from), to, by))
}

fn deadline_checked_range<'a, V: ValT + 'a>(
    mut from: ValX<'a, V>,
    to: V,
    by: V,
) -> impl Iterator<Item = ValX<'a, V>> + 'a {
    use std::cmp::Ordering::{Equal, Greater, Less};

    let cmp = by.partial_cmp(&0isize.into()).unwrap_or(Equal);
    std::iter::from_fn(move || {
        if let Err(err) = check_deadline() {
            from = Ok(to.clone());
            return Some(Err(Exn::from(jaq_core::Error::str(err.to_string()))));
        }
        match from.clone() {
            Ok(x) => match cmp {
                Greater => x < to,
                Less => x > to,
                Equal => x != to,
            }
            .then(|| std::mem::replace(&mut from, (x + by.clone()).map_err(Exn::from))),
            e @ Err(_) => {
                from = Ok(to.clone());
                Some(e)
            }
        }
    })
}

fn check_deadline() -> Result<(), JqError> {
    if CONTROL.with(|control| {
        control.borrow().as_ref().is_some_and(|control| {
            control.cancelled.load(Ordering::Relaxed)
                || control
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
        })
    }) {
        Err(JqError::Runtime("execution timed out".to_owned()))
    } else {
        Ok(())
    }
}

fn jq_exception_to_error(err: jaq_core::Exn<'_, JaqValue>) -> JqError {
    match err.get_err() {
        Ok(err) => JqError::Runtime(err.to_string()),
        Err(err) => match err.get_halt() {
            Ok(code) => JqError::Halt(code),
            Err(err) => JqError::Runtime(format!("{err:?}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use jaq_json::write;
    use serde_json::json;

    use super::{
        JaqValue, JqError, MAX_JQ_FILTER_NESTING, MAX_JQ_FILTER_SYNTAX_TOKENS, compile_with_vars,
        validate_filter_source,
    };

    fn eval(filter: &str, input: serde_json::Value) -> Vec<serde_json::Value> {
        let input = serde_json::from_value(input).unwrap();
        compile_with_vars(filter, &[])
            .unwrap()
            .evaluate_values(input, &[])
            .unwrap()
            .into_iter()
            .map(jaq_to_json)
            .collect()
    }

    fn jaq_to_json(value: JaqValue) -> serde_json::Value {
        let mut buf = Vec::new();
        write::write(&mut buf, &write::Pp::default(), 0, &value).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    #[test]
    fn evaluates_identity_property_access_and_pipes() {
        // Covers the basic filter chain shape that the builtin will use for stdin values.
        let out = eval(".user | .name", json!({ "user": { "name": "Ada" } }));
        assert_eq!(out, vec![json!("Ada")]);
    }

    #[test]
    fn evaluates_array_iteration_select_and_map() {
        // `.[]` streams multiple values, while `map` produces a single transformed array.
        let input = json!([
            { "name": "Ada", "active": true },
            { "name": "Linus", "active": false },
            { "name": "Grace", "active": true }
        ]);

        let streamed = eval(".[] | select(.active) | .name", input.clone());
        assert_eq!(streamed, vec![json!("Ada"), json!("Grace")]);

        let mapped = eval("map(.name)", input);
        assert_eq!(mapped, vec![json!(["Ada", "Linus", "Grace"])]);
    }

    #[test]
    fn evaluates_group_by_to_entries_and_string_interpolation() {
        // These standard jq helpers exercise jaq definitions plus native std/json filters.
        let grouped = eval(
            "group_by(.team) | map({ team: .[0].team, names: map(.name) })",
            json!([
                { "team": "core", "name": "Ada" },
                { "team": "core", "name": "Grace" },
                { "team": "web", "name": "Linus" }
            ]),
        );
        assert_eq!(
            grouped,
            vec![json!([
                { "team": "core", "names": ["Ada", "Grace"] },
                { "team": "web", "names": ["Linus"] }
            ])]
        );

        let entries = eval(
            r#"to_entries | map("\(.key)=\(.value)")"#,
            json!({ "a": 1, "b": "two" }),
        );
        assert_eq!(entries, vec![json!(["a=1", "b=two"])]);
    }

    #[test]
    fn evaluates_std_and_json_definition_backed_filters() {
        // These filters are wrappers from jaq_std::defs and jaq_json::defs, not bare natives.
        assert_eq!(
            eval(r#"split(",")"#, json!("a,b,c")),
            vec![json!(["a", "b", "c"])]
        );
        assert_eq!(eval("tonumber", json!("42")), vec![json!(42)]);
        assert_eq!(eval(r#"inside("abc")"#, json!("b")), vec![json!(true)]);
        assert_eq!(eval("@json", json!({ "a": 1 })), vec![json!(r#"{"a":1}"#)]);
    }

    #[test]
    fn evaluates_global_variables_supplied_by_cli_args() {
        // CLI variables are compiled as jaq globals so filters see real `$name`
        // bindings rather than rewritten source text.
        let program = compile_with_vars(
            "{ name: $name, count: $count + 1 }",
            &["$name".to_owned(), "$count".to_owned()],
        )
        .unwrap();
        let vars = [json!("Ada"), json!(2)]
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let out = program
            .evaluate_values(jaq_json::Val::Null, &vars)
            .unwrap()
            .into_iter()
            .map(jaq_to_json)
            .collect::<Vec<_>>();
        assert_eq!(out, vec![json!({ "name": "Ada", "count": 3 })]);
    }

    #[test]
    fn classifies_compile_errors() {
        let Err(err) = compile_with_vars(".[", &[]) else {
            panic!("invalid jq filter unexpectedly compiled");
        };
        assert!(matches!(err, JqError::Compile(_)));
    }

    #[test]
    fn rejects_user_defined_filters() {
        let Err(err) = compile_with_vars("def f: f + 1; f", &[]) else {
            panic!("user-defined jq filter unexpectedly compiled");
        };
        assert_eq!(
            err,
            JqError::Compile(
                "user-defined jq functions are not supported in tinysandbox".to_owned()
            )
        );
    }

    #[test]
    fn rejects_deep_filter_source_before_jaq_parse() {
        let filter = format!(
            "{}0{}",
            "[".repeat(MAX_JQ_FILTER_NESTING + 1),
            "]".repeat(MAX_JQ_FILTER_NESTING + 1)
        );

        let Err(err) = compile_with_vars(&filter, &[]) else {
            panic!("deep jq filter unexpectedly compiled");
        };
        assert_eq!(
            err,
            JqError::Compile(format!(
                "jq filter nesting exceeds maximum depth {MAX_JQ_FILTER_NESTING}"
            ))
        );
    }

    #[test]
    fn rejects_deep_recursion_driving_filter_source_before_jaq_parse() {
        // These shapes previously reached jaq's recursive parser without
        // increasing delimiter depth, which could overflow the host stack.
        let filters = [
            (
                "unary minus",
                format!("{}0", "-".repeat(MAX_JQ_FILTER_SYNTAX_TOKENS + 1)),
            ),
            (
                "try",
                format!("{}0", "try ".repeat(MAX_JQ_FILTER_SYNTAX_TOKENS + 1)),
            ),
            (
                "alt",
                format!("1{}", "//1".repeat(MAX_JQ_FILTER_SYNTAX_TOKENS + 1)),
            ),
            ("pipe", vec!["."; MAX_JQ_FILTER_SYNTAX_TOKENS + 2].join("|")),
        ];

        for (name, filter) in filters {
            let Err(err) = compile_with_vars(&filter, &[]) else {
                panic!("{name} jq filter unexpectedly compiled");
            };
            assert_eq!(
                err,
                JqError::Compile(format!(
                    "jq filter complexity exceeds maximum token count {MAX_JQ_FILTER_SYNTAX_TOKENS}"
                )),
                "{name}"
            );
        }
    }

    #[test]
    fn compiles_realistic_filter_under_source_bounds() {
        // Representative jq filters should have plenty of room below the source
        // complexity guard while still exercising pipes, objects, arrays, and
        // standard-library helpers.
        compile_with_vars(
            r#"
            .teams
            | map({
                team: .name,
                active: (.members | map(select(.active)) | length),
                names: (.members | map(.name) | sort)
              })
            | sort_by(.team)
            "#,
            &[],
        )
        .expect("realistic jq filter should compile");
    }

    #[test]
    fn rejects_deep_filter_source_inside_string_interpolation() {
        let filter = format!(
            r#""\({}0{})""#,
            "(".repeat(MAX_JQ_FILTER_NESTING + 1),
            ")".repeat(MAX_JQ_FILTER_NESTING + 1)
        );

        let Err(err) = compile_with_vars(&filter, &[]) else {
            panic!("deep jq interpolation unexpectedly compiled");
        };
        assert_eq!(
            err,
            JqError::Compile(format!(
                "jq filter nesting exceeds maximum depth {MAX_JQ_FILTER_NESTING}"
            ))
        );
    }

    #[test]
    fn filter_source_nesting_ignores_strings_comments_and_escapes() {
        let nested_text = "[".repeat(MAX_JQ_FILTER_NESTING + 1);
        validate_filter_source(&format!(r#""{nested_text}""#)).unwrap();

        let comment = format!("# {}\n1", "(".repeat(MAX_JQ_FILTER_NESTING + 1));
        validate_filter_source(&comment).unwrap();

        let escaped_interpolation = format!(r#""{}""#, r"\\(".repeat(MAX_JQ_FILTER_NESTING + 1));
        validate_filter_source(&escaped_interpolation).unwrap();
    }

    #[test]
    fn classifies_runtime_errors() {
        let err = compile_with_vars("length", &[])
            .unwrap()
            .evaluate_values(jaq_json::Val::Bool(true), &[])
            .unwrap_err();
        assert!(matches!(err, JqError::Runtime(_)));
    }
}
