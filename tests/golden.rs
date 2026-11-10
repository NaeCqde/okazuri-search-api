//! Runs the built executable on `tests/fixtures/golden.tsv` (`input<TAB>expected`).
//!
//! The expectations are what the Python server this replaces (cutlet) answers for the same text, taken
//! from cases where the two agree, plus the examples the conversion is made for. The text goes in through
//! the standard input, as it does in the CI smoke test, so it is UTF-8 on every platform.

use std::io::Write;
use std::process::{Command, Stdio};

fn convert(args: &[&str], input: &str) -> Vec<String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_okazuri-search-api"))
        .arg("convert")
        .args(args)
        // a config.toml next to the tests must not change the results
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the executable starts");

    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());

    String::from_utf8(output.stdout).unwrap().lines().map(str::to_string).collect()
}

#[test]
fn matches_the_golden_cases() {
    let golden = include_str!("fixtures/golden.tsv");
    let cases: Vec<(&str, &str)> = golden.lines().filter_map(|line| line.split_once('\t')).collect();
    assert!(cases.len() > 30);

    let input: String = cases.iter().map(|(text, _)| format!("{text}\n")).collect();
    let actual = convert(&[], &input);
    assert_eq!(actual.len(), cases.len());

    let wrong: Vec<String> = cases
        .iter()
        .zip(&actual)
        .filter(|((_, expected), actual)| expected != actual)
        .map(|((text, expected), actual)| format!("{text}\n  expected: {expected}\n  actual:   {actual}"))
        .collect();

    assert!(wrong.is_empty(), "{} of {} differ:\n{}", wrong.len(), cases.len(), wrong.join("\n"));
}

#[test]
fn loanwords_english_is_an_option_of_convert() {
    let actual = convert(&["--loanwords", "english"], "ナカよしふらすとれーしょん！\n");
    assert_eq!(actual, ["naka yoshi frustration!"]);
}

#[test]
fn an_empty_line_stays_an_empty_line() {
    let actual = convert(&[], "あ\n\nい\n");
    assert_eq!(actual, ["a", "", "i"]);
}
