//! Small helpers shared by the modules.

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

static LOG_QUERIES: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Sets which log lines have the text of a query (`[log] queries`); the config sets it when it is read.
pub fn set_log_queries(mode: crate::config::LogQueries) {
    LOG_QUERIES.store(mode as u8, std::sync::atomic::Ordering::Relaxed);
}

/// Whether a log line of this level (`"info"`, `"warn"`...) has the text of a query: never (`off`), only a failure (`errors`: warnings and
/// errors), or every line (`all`).
pub fn log_text(level: &str) -> bool {
    use crate::config::LogQueries;

    match LOG_QUERIES.load(std::sync::atomic::Ordering::Relaxed) {
        x if x == LogQueries::All as u8 => true,
        x if x == LogQueries::Errors as u8 => matches!(level, "warn" | "error"),
        _ => false,
    }
}

/// Seconds since 1970-01-01 UTC.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The time of each stage of a piece of work: `lap` names the stage that just ended, and `Display` is "stage 12 ms, stage 3 ms".
pub struct Laps {
    last: std::time::Instant,
    stages: Vec<(&'static str, u128)>,
}

impl Laps {
    pub fn new() -> Self {
        Self { last: std::time::Instant::now(), stages: Vec::new() }
    }

    pub fn lap(&mut self, stage: &'static str) {
        self.stages.push((stage, self.last.elapsed().as_millis()));
        self.last = std::time::Instant::now();
    }
}

impl std::fmt::Display for Laps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total: u128 = self.stages.iter().map(|(_, ms)| ms).sum();

        for (stage, ms) in &self.stages {
            write!(f, "{stage} {ms} ms, ")?;
        }

        write!(f, "total {total} ms")
    }
}

/// The distance between a hiragana and the katakana of the same sound.
const KANA_OFFSET: u32 = 0x60;

/// Katakana to hiragana (`jaconv.kata2hira`): the letters U+30A1..=U+30F6 and the iteration marks
/// U+30FD / U+30FE move by 0x60. Everything else, the prolonged sound mark included, is left as it is.
pub fn kata_to_hira(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{30A1}'..='\u{30F6}' | '\u{30FD}' | '\u{30FE}' => shift(c, false),
            _ => c,
        })
        .collect()
}

/// Hiragana to katakana (`jaconv.hira2kata`), the other way round.
pub fn hira_to_kata(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{3041}'..='\u{3096}' | '\u{309D}' | '\u{309E}' => shift(c, true),
            _ => c,
        })
        .collect()
}

fn shift(c: char, up: bool) -> char {
    let code = if up { c as u32 + KANA_OFFSET } else { c as u32 - KANA_OFFSET };
    char::from_u32(code).unwrap_or(c)
}

pub fn is_hiragana_char(c: char) -> bool {
    ('\u{3041}'..='\u{3096}').contains(&c)
}

pub fn is_katakana_char(c: char) -> bool {
    ('\u{30A1}'..='\u{30FA}').contains(&c)
}

/// Only hiragana (`[ぁ-ゖ]+`).
pub fn is_hiragana(text: &str) -> bool {
    !text.is_empty() && text.chars().all(is_hiragana_char)
}

/// Hiragana, katakana, the prolonged sound mark and the middle dot between katakana words
/// (`[ぁ-ゖァ-ヺー・]+`). MeCab puts all of them in the kana character types.
pub fn is_kana(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| is_hiragana_char(c) || is_katakana_char(c) || c == 'ー' || c == '・')
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_query_is_in_a_log_line_as_the_setting_says() {
        use crate::config::LogQueries;

        set_log_queries(LogQueries::Off);
        assert!(!log_text("info") && !log_text("warn") && !log_text("error"));
        set_log_queries(LogQueries::Errors);
        assert!(!log_text("info") && !log_text("debug") && log_text("warn") && log_text("error"));
        set_log_queries(LogQueries::All);
        assert!(log_text("info") && log_text("warn"));
        set_log_queries(LogQueries::Off);
    }

    use super::*;

    #[test]
    fn converts_between_the_kana() {
        assert_eq!(kata_to_hira("チートアイテム"), "ちーとあいてむ");
        assert_eq!(hira_to_kata("ふらすとれーしょん"), "フラストレーション");
        assert_eq!(kata_to_hira("ヴ"), "ゔ");
        // the mark and other scripts are not touched
        assert_eq!(kata_to_hira("ABCー漢字"), "ABCー漢字");
    }

    #[test]
    fn tells_the_scripts_apart() {
        assert!(is_hiragana("ふぁみこん"));
        assert!(!is_hiragana("ふぁミコン"));
        assert!(!is_hiragana("ふー"));
        assert!(is_kana("ふーミコン"));
        assert!(!is_kana("ふ漢"));
        assert!(!is_kana(""));
    }
}
