use thinbox::shell::{ParseErrorKind, QuoteKind, UnsupportedConstruct, parse};

#[test]
fn unsupported_constructs_report_the_construct_position() {
    // Each assertion fixes the byte offset that the executor can show in user-facing errors.
    let cases = [
        (
            "echo *",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::Glob),
        ),
        (
            "echo a?",
            6,
            ParseErrorKind::Unsupported(UnsupportedConstruct::Glob),
        ),
        (
            "echo [ab]",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::Glob),
        ),
        (
            "echo $(date)",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::CommandSubstitution),
        ),
        (
            "echo `date`",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::Backticks),
        ),
        (
            "sleep 1 &",
            8,
            ParseErrorKind::Unsupported(UnsupportedConstruct::Background),
        ),
        (
            "(echo hi)",
            0,
            ParseErrorKind::Unsupported(UnsupportedConstruct::Subshell),
        ),
        (
            "{ echo; }",
            0,
            ParseErrorKind::Unsupported(UnsupportedConstruct::BraceExpansion),
        ),
        (
            "cat <<EOF",
            4,
            ParseErrorKind::Unsupported(UnsupportedConstruct::Heredoc),
        ),
        (
            "echo ~",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::TildeExpansion),
        ),
        (
            "echo $1",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::ParameterExpansion),
        ),
        (
            "echo $$",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::ParameterExpansion),
        ),
        (
            "echo $_",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::ParameterExpansion),
        ),
        (
            "echo ${5}",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::ParameterExpansion),
        ),
        (
            "echo $'a\\tb'",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::AnsiCString),
        ),
        (
            "echo $\"msg\"",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::LocaleTranslation),
        ),
        (
            "echo a=~",
            7,
            ParseErrorKind::Unsupported(UnsupportedConstruct::TildeExpansion),
        ),
        (
            "VAR=~",
            4,
            ParseErrorKind::Unsupported(UnsupportedConstruct::TildeExpansion),
        ),
        (
            "v=a:~",
            4,
            ParseErrorKind::Unsupported(UnsupportedConstruct::TildeExpansion),
        ),
        (
            "VAR+=x",
            3,
            ParseErrorKind::Unsupported(UnsupportedConstruct::AppendAssignment),
        ),
        (
            "V\\\nAR+=x",
            5,
            ParseErrorKind::Unsupported(UnsupportedConstruct::AppendAssignment),
        ),
        (
            "! echo",
            0,
            ParseErrorKind::Unsupported(UnsupportedConstruct::PipelineNegation),
        ),
        (
            "echo hi | ! wc",
            10,
            ParseErrorKind::Unsupported(UnsupportedConstruct::PipelineNegation),
        ),
    ];

    for (input, position, kind) in cases {
        let error = parse(input).expect_err(input);
        assert_eq!(error.position, position, "input: {input}");
        assert_eq!(error.kind, kind, "input: {input}");
    }
}

#[test]
fn incomplete_quotes_and_expansions_report_opening_position() {
    // Unterminated constructs point at the byte that started the construct.
    let cases = [
        (
            "echo 'abc",
            5,
            ParseErrorKind::UnterminatedQuote {
                quote: QuoteKind::Single,
            },
        ),
        (
            "echo \"abc",
            5,
            ParseErrorKind::UnterminatedQuote {
                quote: QuoteKind::Double,
            },
        ),
        (
            "echo ${HOME",
            5,
            ParseErrorKind::UnterminatedBracedExpansion,
        ),
        (
            "echo ${HOME:-x}",
            5,
            ParseErrorKind::InvalidParameterExpansion,
        ),
    ];

    for (input, position, kind) in cases {
        let error = parse(input).expect_err(input);
        assert_eq!(error.position, position, "input: {input}");
        assert_eq!(error.kind, kind, "input: {input}");
    }
}

#[test]
fn missing_commands_and_redirect_targets_are_structured_errors() {
    // These are grammar errors rather than unsupported syntax.
    let cases = [
        (
            "| foo",
            0,
            ParseErrorKind::UnexpectedToken {
                expected: "command",
                found: "`|`",
            },
        ),
        (
            "foo |",
            5,
            ParseErrorKind::UnexpectedEof {
                expected: "command after pipe",
            },
        ),
        (
            "a &&",
            4,
            ParseErrorKind::UnexpectedEof {
                expected: "command after `&&`",
            },
        ),
        ("echo >", 5, ParseErrorKind::MissingRedirectTarget),
        (
            "echo hi 2>& 1",
            9,
            ParseErrorKind::Unsupported(UnsupportedConstruct::RedirectFdDup),
        ),
        (
            ">&file",
            0,
            ParseErrorKind::Unsupported(UnsupportedConstruct::RedirectFdDup),
        ),
        (
            "<&1",
            0,
            ParseErrorKind::Unsupported(UnsupportedConstruct::RedirectFdDup),
        ),
        ("cmd 2>&1x", 5, ParseErrorKind::AmbiguousRedirect),
    ];

    for (input, position, kind) in cases {
        let error = parse(input).expect_err(input);
        assert_eq!(error.position, position, "input: {input}");
        assert_eq!(error.kind, kind, "input: {input}");
    }
}

#[test]
fn parse_error_display_includes_position_and_message() {
    let cases = [
        (
            "echo ${HOME:-x}",
            "parse error at byte 5: invalid or unsupported parameter expansion",
        ),
        ("cmd 2>&1x", "parse error at byte 5: ambiguous redirect"),
        (
            "VAR+=x",
            "parse error at byte 3: unsupported append assignment",
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(parse(input).expect_err(input).to_string(), expected);
    }
}
