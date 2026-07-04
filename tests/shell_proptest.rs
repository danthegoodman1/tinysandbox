use proptest::prelude::*;
use thinbox::shell::{Command, Segment, parse};

proptest! {
    #[test]
    fn shellish_input_never_panics(input in shellish_input()) {
        // The parser boundary is Result-based even for dense shell metacharacter streams.
        let _ = parse(&input);
    }

    #[test]
    fn rendered_plain_simple_commands_round_trip_words(words in plain_words()) {
        // Plain words avoid shell metacharacters, so parsing should preserve exact literal text.
        let input = words.join(" ");
        let program = parse(&input).expect("rendered plain command should parse");

        let [list] = program.lists.as_slice() else {
            panic!("expected one list: {program:?}");
        };
        let [command] = list.first.commands.as_slice() else {
            panic!("expected one command: {program:?}");
        };
        let Command::Simple(command) = command;

        let parsed_words: Vec<_> = command
            .words
            .iter()
            .map(|word| match word.segments.as_slice() {
                [Segment::Literal {
                    value,
                    quoted: false,
                }] => value.clone(),
                segments => panic!("expected one unquoted literal segment: {segments:?}"),
            })
            .collect();

        assert_eq!(parsed_words, words);
    }
}

fn shellish_input() -> impl Strategy<Value = String> {
    const ALPHABET: &[char] = &[
        '\'', '"', '\\', '$', '|', '&', ';', '>', '<', '#', '{', '}', '(', ')', '*', '~', ' ',
        '\t', '\n', 'a', 'b', 'c', 'X', 'Y', 'Z', '_', '0', '1', '=', '+', '?', '[', ']',
    ];

    prop::collection::vec(prop::sample::select(ALPHABET), 0..256)
        .prop_map(|chars| chars.into_iter().collect())
}

fn plain_words() -> impl Strategy<Value = Vec<String>> {
    let word_char = prop::sample::select(
        "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_./:-"
            .chars()
            .collect::<Vec<_>>(),
    );
    let word = prop::collection::vec(word_char, 1..16)
        .prop_map(|chars| chars.into_iter().collect::<String>());

    prop::collection::vec(word, 1..12)
}
