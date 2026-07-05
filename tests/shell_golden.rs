use tinysandbox::shell::{
    AndOr, AndOrList, AndOrOp, Assignment, Command, Pipeline, Program, Redirect, RedirectOp,
    RedirectTarget, Segment, SimpleCommand, Word, parse,
};

#[test]
fn golden_corpus_matches_expected_ast() {
    // These cases pin bash-compatible word formation separately from expansion/execution.
    let cases = vec![
        ("", Program::default()),
        (
            "echo \"a b\"",
            one(simple_words(vec![lit_word("echo"), qword("a b")])),
        ),
        (
            "echo 'a$B'",
            one(simple_words(vec![lit_word("echo"), qword("a$B")])),
        ),
        (
            "echo a\"b c\"d",
            one(simple_words(vec![
                lit_word("echo"),
                word(vec![lit("a"), qlit("b c"), lit("d")]),
            ])),
        ),
        (
            "echo \\$HOME",
            one(simple_words(vec![
                lit_word("echo"),
                word(vec![qlit("$"), lit("HOME")]),
            ])),
        ),
        (
            "echo \"\\$HOME\"",
            one(simple_words(vec![lit_word("echo"), qword("$HOME")])),
        ),
        (
            "echo \"foo$\"",
            one(simple_words(vec![lit_word("echo"), qword("foo$")])),
        ),
        (
            "echo \"$'x'\"",
            one(simple_words(vec![lit_word("echo"), qword("$'x'")])),
        ),
        (
            "echo \"a\\\\b\"",
            one(simple_words(vec![lit_word("echo"), qword("a\\b")])),
        ),
        (
            "echo \"\" end",
            one(simple_words(vec![
                lit_word("echo"),
                qword(""),
                lit_word("end"),
            ])),
        ),
        (
            "echo a\"\"b",
            one(simple_words(vec![
                lit_word("echo"),
                word(vec![lit("a"), qlit(""), lit("b")]),
            ])),
        ),
        (
            "echo ''",
            one(simple_words(vec![lit_word("echo"), qword("")])),
        ),
        (
            "echo a\\ b",
            one(simple_words(vec![
                lit_word("echo"),
                word(vec![lit("a"), qlit(" "), lit("b")]),
            ])),
        ),
        (
            "echo $HOME \"$HOME\" ${USER}",
            one(simple_words(vec![
                lit_word("echo"),
                word(vec![expansion("HOME", false)]),
                word(vec![expansion("HOME", true)]),
                word(vec![expansion("USER", false)]),
            ])),
        ),
        (
            "echo pre${X}post",
            one(simple_words(vec![
                lit_word("echo"),
                word(vec![lit("pre"), expansion("X", false), lit("post")]),
            ])),
        ),
        (
            "echo hi # comment",
            one(simple_words(vec![lit_word("echo"), lit_word("hi")])),
        ),
        (
            "VAR=x",
            one(simple(
                vec![assignment("VAR", lit_word("x"))],
                Vec::new(),
                Vec::new(),
            )),
        ),
        (
            "VAR=x cmd",
            one(simple(
                vec![assignment("VAR", lit_word("x"))],
                vec![lit_word("cmd")],
                Vec::new(),
            )),
        ),
        (
            "VAR =x",
            one(simple_words(vec![lit_word("VAR"), lit_word("=x")])),
        ),
        (
            "echo hi >file",
            one(simple(
                Vec::new(),
                vec![lit_word("echo"), lit_word("hi")],
                vec![redirect(
                    None,
                    RedirectOp::Write,
                    RedirectTarget::Word(lit_word("file")),
                )],
            )),
        ),
        (
            "echo hi > file",
            one(simple(
                Vec::new(),
                vec![lit_word("echo"), lit_word("hi")],
                vec![redirect(
                    None,
                    RedirectOp::Write,
                    RedirectTarget::Word(lit_word("file")),
                )],
            )),
        ),
        (
            "echo hi 2> f",
            one(simple(
                Vec::new(),
                vec![lit_word("echo"), lit_word("hi")],
                vec![redirect(
                    Some(2),
                    RedirectOp::Write,
                    RedirectTarget::Word(lit_word("f")),
                )],
            )),
        ),
        (
            "0<f",
            one(simple(
                Vec::new(),
                Vec::new(),
                vec![redirect(
                    Some(0),
                    RedirectOp::Read,
                    RedirectTarget::Word(lit_word("f")),
                )],
            )),
        ),
        (
            "cat <in 2>>log",
            one(simple(
                Vec::new(),
                vec![lit_word("cat")],
                vec![
                    redirect(None, RedirectOp::Read, RedirectTarget::Word(lit_word("in"))),
                    redirect(
                        Some(2),
                        RedirectOp::Append,
                        RedirectTarget::Word(lit_word("log")),
                    ),
                ],
            )),
        ),
        (
            "cmd 2>&1",
            one(simple(
                Vec::new(),
                vec![lit_word("cmd")],
                vec![redirect(Some(2), RedirectOp::Write, RedirectTarget::Fd(1))],
            )),
        ),
        (
            "a && b || c; d",
            Program {
                lists: vec![
                    AndOrList {
                        first: pipeline(vec![simple_words(vec![lit_word("a")])]),
                        rest: vec![
                            AndOr {
                                op: AndOrOp::And,
                                pipeline: pipeline(vec![simple_words(vec![lit_word("b")])]),
                            },
                            AndOr {
                                op: AndOrOp::Or,
                                pipeline: pipeline(vec![simple_words(vec![lit_word("c")])]),
                            },
                        ],
                    },
                    AndOrList {
                        first: pipeline(vec![simple_words(vec![lit_word("d")])]),
                        rest: Vec::new(),
                    },
                ],
            },
        ),
        (
            "a &&\nb",
            Program {
                lists: vec![AndOrList {
                    first: pipeline(vec![simple_words(vec![lit_word("a")])]),
                    rest: vec![AndOr {
                        op: AndOrOp::And,
                        pipeline: pipeline(vec![simple_words(vec![lit_word("b")])]),
                    }],
                }],
            },
        ),
        (
            "left | right",
            Program {
                lists: vec![AndOrList {
                    first: pipeline(vec![
                        simple_words(vec![lit_word("left")]),
                        simple_words(vec![lit_word("right")]),
                    ]),
                    rest: Vec::new(),
                }],
            },
        ),
        (
            "a |\nb",
            Program {
                lists: vec![AndOrList {
                    first: pipeline(vec![
                        simple_words(vec![lit_word("a")]),
                        simple_words(vec![lit_word("b")]),
                    ]),
                    rest: Vec::new(),
                }],
            },
        ),
        (
            "left | middle | right",
            Program {
                lists: vec![AndOrList {
                    first: pipeline(vec![
                        simple_words(vec![lit_word("left")]),
                        simple_words(vec![lit_word("middle")]),
                        simple_words(vec![lit_word("right")]),
                    ]),
                    rest: Vec::new(),
                }],
            },
        ),
        (
            "echo done;",
            one(simple_words(vec![lit_word("echo"), lit_word("done")])),
        ),
        (
            "echo first\necho second",
            Program {
                lists: vec![
                    AndOrList {
                        first: pipeline(vec![simple_words(vec![
                            lit_word("echo"),
                            lit_word("first"),
                        ])]),
                        rest: Vec::new(),
                    },
                    AndOrList {
                        first: pipeline(vec![simple_words(vec![
                            lit_word("echo"),
                            lit_word("second"),
                        ])]),
                        rest: Vec::new(),
                    },
                ],
            },
        ),
        (
            "echo before # comment\necho after",
            Program {
                lists: vec![
                    AndOrList {
                        first: pipeline(vec![simple_words(vec![
                            lit_word("echo"),
                            lit_word("before"),
                        ])]),
                        rest: Vec::new(),
                    },
                    AndOrList {
                        first: pipeline(vec![simple_words(vec![
                            lit_word("echo"),
                            lit_word("after"),
                        ])]),
                        rest: Vec::new(),
                    },
                ],
            },
        ),
        (
            "echo $?",
            one(simple_words(vec![
                lit_word("echo"),
                word(vec![expansion("?", false)]),
            ])),
        ),
        (
            "echo \"\"$X $X",
            one(simple_words(vec![
                lit_word("echo"),
                word(vec![qlit(""), expansion("X", false)]),
                word(vec![expansion("X", false)]),
            ])),
        ),
        (
            "echo a:~",
            one(simple_words(vec![lit_word("echo"), lit_word("a:~")])),
        ),
    ];

    let fixture_inputs: Vec<_> = include_str!("fixtures/shell_golden.txt")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(decode_fixture_case)
        .collect();
    let case_inputs: Vec<_> = cases.iter().map(|(input, _)| (*input).to_owned()).collect();
    assert_eq!(fixture_inputs, case_inputs);

    for (input, expected) in cases {
        assert_eq!(parse(input), Ok(expected), "input: {input}");
    }
}

#[test]
fn special_status_expansion_is_kept_for_executor_state() {
    // The executor owns the actual status value; the parser only preserves the expansion site.
    assert_eq!(
        parse("echo $?"),
        Ok(one(simple_words(vec![
            lit_word("echo"),
            word(vec![expansion("?", false)])
        ])))
    );
}

fn decode_fixture_case(line: &str) -> String {
    if line == "<empty>" {
        String::new()
    } else {
        line.replace("\\n", "\n")
    }
}

fn one(command: Command) -> Program {
    Program {
        lists: vec![AndOrList {
            first: pipeline(vec![command]),
            rest: Vec::new(),
        }],
    }
}

fn pipeline(commands: Vec<Command>) -> Pipeline {
    Pipeline { commands }
}

fn simple_words(words: Vec<Word>) -> Command {
    simple(Vec::new(), words, Vec::new())
}

fn simple(assignments: Vec<Assignment>, words: Vec<Word>, redirects: Vec<Redirect>) -> Command {
    Command::Simple(SimpleCommand {
        assignments,
        words,
        redirects,
    })
}

fn assignment(name: &str, value: Word) -> Assignment {
    Assignment {
        name: name.to_owned(),
        value,
    }
}

fn redirect(fd: Option<u32>, op: RedirectOp, target: RedirectTarget) -> Redirect {
    Redirect { fd, op, target }
}

fn lit_word(value: &str) -> Word {
    word(vec![lit(value)])
}

fn qword(value: &str) -> Word {
    word(vec![qlit(value)])
}

fn expansion(name: &str, quoted: bool) -> Segment {
    Segment::Expansion {
        name: name.to_owned(),
        quoted,
    }
}

fn lit(value: &str) -> Segment {
    Segment::Literal {
        value: value.to_owned(),
        quoted: false,
    }
}

fn qlit(value: &str) -> Segment {
    Segment::Literal {
        value: value.to_owned(),
        quoted: true,
    }
}

fn word(segments: Vec<Segment>) -> Word {
    Word { segments }
}
