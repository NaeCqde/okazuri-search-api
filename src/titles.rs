//! The title of a work from the address of its page (`GET /okazuri/title`): the address is matched with the pattern of the
//! site (the one of `okazuri_bot/src/websites.ts`), cleaned, the page is asked for, and the title is read from it the way the
//! bot reads it. Hitomi.La has no entry: its gallery file is read in `engine.rs`.

use regex::Regex;
use std::sync::OnceLock;

/// How the title of a site is found in its page: the expression has a group `title`, and a group `artist` that is put
/// after the title when the page has it.
const PATTERNS: &[(&str, &str)] = &[
    ("DLsite", r#"<meta property="og:title" content="(?<title>.+) \| DLsite"#),
    ("FANZA", r#"<meta property="og:title" content="(?<title>.+)(?:" />|\n">)"#),
    ("Anime-Sharing", r"<title>(?<title>.+) \| Anime-Sharing Community</title>"),
    ("IMHentai", r"<h1>(?<title>.+)</h1>"),
    (
        "momon:GA",
        r#"<h1>(?<title>.+)</h1>(?:(?:\n|\t|.)+作者</div>.+https://momon-ga\.com/cartoonist/[^ ]+ rel="tag">(?<artist>[^<]+)?</a>)"#,
    ),
    (
        "Nya:Hentai",
        r#"<h1>(?<title>.+)</h1>(?:(?:\n|\t|.)+/circle/[^"]+" rel="tag">(?<circle>[^"]+)?</a>(?:\n|\t|.)+/artist/[^"]+" rel="tag">(?<artist>[^"]+)?</a>)?"#,
    ),
    ("同人の森", r"<title>(?<title>.+) \| 同人の森"),
    ("JComic", r#"<h1>(?<title>.+)</h1>(?:<a href="/author/(?<artist>[^"]+)?")?"#),
    ("R18Best", r"<h1>(?<title>.+)</h1>"),
    ("HentaiOMG", r"<h1>(?<title>.+)</h1>"),
    ("HentaiCore", r"<h1>(?<title>.+)</h1>"),
    ("Sukebei", r"<title>(?<title>.+) \| Sukebei \| Nyaa"),
    ("NijiGazo", r"<title>(?<title>.+) \| NijiGazo"),
    ("RokuHentai", r"<h1>(?<title>.+)</h1>"),
    ("HentaiEnvy", r"<h1>(?<title>.+)</h1>"),
    ("HentaiEra", r"<h1>(?<title>.+)</h1>"),
    ("MasDr", r"<h1>(?<title>.+)</h1>"),
    ("JMwu", r"<h1>(?<title>.+)</h1>"),
    ("NukiBooks", r"<h1>(?<title>.+)</h1>"),
    ("177Picyy", r"<h1>(?<title>.+)</h1>"),
    ("BatoTo", r"<h1>(?<title>.+)</h1>"),
    ("Wnacg", r"<h1>(?<title>.+)</h1>"),
    ("HentaiName", r"<h1>(?<title>.+)</h1>"),
    ("HentaiDesi", r"<h1>(?<title>.+)</h1>"),
    ("AsmHentai", r"<h1>(?<title>.+)</h1>"),
    ("HentaiPaw", r"<h1>(?<title>.+)</h1>"),
    ("EHentai", r"<h1>(?<title>.+)</h1>"),
];

fn compiled() -> &'static Vec<(&'static str, Regex)> {
    static COMPILED: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();

    COMPILED.get_or_init(|| {
        PATTERNS
            .iter()
            .map(|(name, pattern)| (*name, Regex::new(pattern).unwrap_or_else(|e| panic!("the title pattern of {name} is wrong: {e}"))))
            .collect()
    })
}

/// The title in the page of a site, with the artist after it when the page has one. `None` when the page has no title.
pub fn title_of_page(site: &str, page: &str) -> Option<String> {
    let (_, pattern) = compiled().iter().find(|(name, _)| *name == site)?;
    let found = pattern.captures(page)?;
    let title = found.name("title")?.as_str();
    let mut text = title.to_string();

    if let Some(artist) = found.name("artist") {
        text.push(' ');
        text.push_str(artist.as_str());
    }

    let text = text.trim();

    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pattern_compiles_and_belongs_to_a_site() {
        let sites = crate::sites::all();

        for (name, _) in PATTERNS {
            assert!(sites.iter().any(|site| site.def.name == *name), "{name} is not a site");
        }
        assert_eq!(compiled().len(), PATTERNS.len());
    }

    #[test]
    fn the_title_is_read_the_way_the_bot_reads_it() {
        assert_eq!(
            title_of_page("DLsite", r#"<meta property="og:title" content="COMIC快楽天 2026年07月号 | DLsite" />"#).as_deref(),
            Some("COMIC快楽天 2026年07月号")
        );
        assert_eq!(title_of_page("Wnacg", "<h1>作品名</h1>").as_deref(), Some("作品名"));
        assert_eq!(title_of_page("JComic", r#"<h1>作品名</h1><a href="/author/作者""#).as_deref(), Some("作品名 作者"));
        assert_eq!(title_of_page("Sukebei", "<title>名前 | Sukebei | Nyaa</title>").as_deref(), Some("名前"));
        assert_eq!(title_of_page("Wnacg", "<p>no title</p>"), None);
        assert_eq!(title_of_page("Unknown", "<h1>x</h1>"), None);
    }
}
