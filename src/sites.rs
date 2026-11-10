//! The websites the search engine searches: the same list as `okazuri_bot/src/websites.ts`.
//!
//! Every site has a search of its own. The engine asks it for the query and reads the links on the
//! page it answers with: a link is a result when it matches the site's `pattern` (the one
//! `websites.ts` uses to recognize a work's page). So a site needs only a search URL here.
//!
//! What is kept of a result is what the list page says: the title as the page writes it, and the
//! path of the work's page (no scheme or domain). No image, and no address of an image.
//!
//! Not every site can be searched with a plain HTTP request. `Support` says why, and the engine
//! leaves those sites out unless `[search] try_unsupported = true`.

use crate::query::has_japanese;
use regex::{Regex, RegexBuilder};
use std::sync::OnceLock;
use url::Url;

/// Whether the site can be searched by a plain HTTP request (checked in 2026-10 from a home IP).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Answers the search with a page whose links are the results.
    Ready,
    /// Answers with a Cloudflare challenge ("Just a moment..."), which needs a browser.
    Cloudflare,
    /// The page is made by JavaScript, or the search is a POST with a token.
    JavaScript,
    /// The site is closed, or does not answer.
    Closed,
}

impl Support {
    pub fn as_str(self) -> &'static str {
        match self {
            Support::Ready => "ready",
            Support::Cloudflare => "cloudflare",
            Support::JavaScript => "javascript",
            Support::Closed => "closed",
        }
    }
}

/// How the address of a result is tidied up, so that one work has one address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clean {
    /// Only the `#fragment` is cut off.
    Plain,
    /// `?locale=ja_JP` is removed (DLsite).
    DeleteLocale,
    /// `/view/123/1/` is the reader of `/gallery/123/` (IMHentai).
    ViewToGallery,
    /// `/eps/name/3` is a chapter of `/page/name` (JComic).
    EpsToPage,
    /// `/id/0` is the reader of the work `/id` (RokuHentai).
    ReaderToDetails,
}

/// What the search of a site answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A page of HTML.
    Html,
    /// JSON with the cards of the results, as HTML, in the array of this key (RokuHentai: the page is drawn
    /// by JavaScript from this answer).
    JsonCards(&'static str),
}

/// Where the list page of a site keeps the title of a work. A link whose card has the price, the number of
/// pages or the date in its text would give those with the title; this reads only the title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Title {
    /// The text of the link (the title attribute, or the alt of its image, when it has no text).
    Text,
    /// The alt of the image of the link.
    ImgAlt,
    /// The text of the first element of this tag in the link.
    Tag(&'static str),
    /// The text of the first element in the link that has this class.
    Class(&'static str),
    /// The text of a link marked rel="bookmark" (the others, such as "Read More", say nothing).
    Bookmark,
}

/// The sites whose title is not the text of the link.
const TITLES: &[(&str, Title)] = &[
    ("FANZA", Title::Class("line-clamp-2")),
    ("momon:GA", Title::ImgAlt),
    ("Nya:Hentai", Title::Tag("span")),
    ("JComic", Title::Class("comic-title")),
    ("R18Best", Title::Bookmark),
    ("RokuHentai", Title::Class("site-manga-card__title--primary")),
];

pub struct SiteDef {
    pub name: &'static str,
    /// A site that sells the work, not one that copies it.
    pub legal: bool,
    /// The scheme and domain of the pages of works. Only the path is saved; this puts the address back.
    pub origin: &'static str,
    /// The parameters of the query string that name the work (同人の森: `uuid`). The others are dropped.
    pub keep_query: &'static [&'static str],
    /// The address of a work's page (`websites.ts`), tested against every link of the search page.
    pub pattern: &'static str,
    /// A second test for sites whose `pattern` is wider than the results (FANZA's is every page).
    pub filter: Option<&'static str>,
    /// The search page. `{q}` is the query, percent-encoded; `{q+}` writes a space as `+`; `{p}` and `{p+}` are those with the query
    /// in double quotes, unless it has Japanese in it.
    pub search: Option<&'static str>,
    /// Cookies that get past an age check.
    pub cookie: Option<&'static str>,
    pub clean: Clean,
    pub format: Format,
    /// The shortest time between two requests to this site, when it is longer than `[search] min_delay_ms` (a site that
    /// answers 429 to a rush).
    pub delay_ms: u64,
    /// The search finds works by their Japanese title: a query in Japanese is asked as it is (and in romaji as
    /// well). A site that does not (checked with 快楽天 in 2026-10) is only asked the romaji.
    pub japanese: bool,
    /// The search needs `×` as it is written (DLsite, FANZA); the other sites are asked `x`.
    pub keep_times: bool,
    pub support: Support,
    pub note: &'static str,
}

const AGE: &str = "adultchecked=1; age_check_done=1; nw=1; ckcy=1; is_intarnal=true";

const fn site(
    name: &'static str,
    legal: bool,
    origin: &'static str,
    pattern: &'static str,
    search: Option<&'static str>,
    support: Support,
    note: &'static str,
) -> SiteDef {
    SiteDef {
        name,
        legal,
        origin,
        keep_query: &[],
        pattern,
        filter: None,
        search,
        cookie: Some(AGE),
        clean: Clean::Plain,
        format: Format::Html,
        delay_ms: 0,
        japanese: true,
        keep_times: false,
        support,
        note,
    }
}

pub const SITES: &[SiteDef] = &[
    SiteDef {
        clean: Clean::DeleteLocale,
        keep_times: true,
        ..site(
            "DLsite",
            true,
            "https://www.dlsite.com",
            r"^https://(?:[A-Za-z]+\.)?dlsite\.com/[a-z\-]+/work/=/product_id/(?:RJ|BJ)\d+\.html.*",
            Some("https://www.dlsite.com/books/fsr/=/language/jp/sex_category[0]/male/keyword/{p+}/ana_flg/all/order/release_d/options_and_or/and/per_page/30"),
            Support::Ready,
            "",
        )
    },
    SiteDef {
        filter: Some(r"^https://book\.dmm\.co\.jp/product/"),
        keep_times: true,
        ..site(
            "FANZA",
            true,
            "https://book.dmm.co.jp",
            r"^https://(?:[a-z]+\.)?dmm\.co\.jp/[a-z]+/[^/]+/.+",
            Some("https://www.dmm.co.jp/search/=/searchstr={p}/"),
            Support::Ready,
            "the pattern matches every page of the site, so only product pages of the book store count",
        )
    },
    site(
        "Hitomi.La",
        false,
        "https://hitomi.la",
        r"^https://hitomi\.la/[a-z]+/.+",
        None,
        Support::JavaScript,
        "analysed (2026-10): the search is JavaScript over binary index files (SHA-256 of each word, a B-tree read with Range requests on ltn.gold-usergeneratedcontent.net), which gives gallery ids; the titles are one more file per id. Possible, but dozens of requests for one query, so it is not done",
    ),
    site(
        "Anime-Sharing",
        false,
        "https://www.anime-sharing.com",
        r"^https://www\.anime-sharing\.com/threads/.+",
        None,
        Support::JavaScript,
        "analysed (2026-10): the search is a POST with a token (XenForo), and the site answers a guest with its own 503 page, so it cannot be searched without logging in",
    ),
    SiteDef {
        clean: Clean::ViewToGallery,
        ..site(
            "IMHentai",
            false,
            "https://imhentai.xxx",
            r"^https://imhentai\.xxx/[a-z]+/\d+.*",
            Some("https://imhentai.xxx/search/?key={q}"),
            Support::Cloudflare,
            "",
        )
    },
    site(
        "momon:GA",
        false,
        "https://momon-ga.com",
        r"^https://momon-ga\.com/(?:fanzine|magazine)/mo\d+.*",
        Some("https://momon-ga.com/?s={q}"),
        Support::Ready,
        "",
    ),
    site(
        "Nya:Hentai",
        false,
        "https://nyahentai.one",
        r"^https://nyahentai\.(?:re|one)/(?:fanzine|magazine)/re\d+.*",
        Some("https://nyahentai.one/?s={q}"),
        Support::Ready,
        "moved from nyahentai.re to nyahentai.one; both addresses are accepted",
    ),
    SiteDef {
        keep_query: &["uuid"],
        ..site(
            "同人の森",
            false,
            "https://doujinnomori.com",
            r"^https://doujinnomori\.com/comics/detail/?\?uuid=[a-z\d\-]+.*",
            Some("https://doujinnomori.com/comics/free-search?keyword={q}"),
            Support::Ready,
            "",
        )
    },
    SiteDef {
        clean: Clean::EpsToPage,
        ..site(
            "JComic",
            false,
            "https://jcomic.net",
            r"^https://jcomic\.net/(?:page|eps)/[^/]+.*",
            Some("https://jcomic.net/search/{q}"),
            Support::Ready,
            "",
        )
    },
    SiteDef {
        delay_ms: 5000,
        ..site(
            "R18Best",
            false,
            "https://r18.best",
            r"^https://r18\.best/\d+.*",
            Some("https://r18.best/?s={q}"),
            Support::Ready,
            "answers 429 to requests less than a few seconds apart",
        )
    },
    site(
        "HentaiOMG",
        false,
        "https://hentaiomg.com",
        r"^https://hentaiomg\.com/[^\s]+",
        Some("https://hentaiomg.com/?s={q}"),
        Support::Cloudflare,
        "",
    ),
    site(
        "HentaiCore",
        false,
        "https://www.hentaicore.net",
        r"^https://www\.hentaicore\.net/[a-z]+/\d+.*",
        Some("https://www.hentaicore.net/?s={q}"),
        Support::Cloudflare,
        "",
    ),
    site(
        "Sukebei",
        false,
        "https://sukebei.nyaa.si",
        r"^https://sukebei\.nyaa\.si/view/(?:\d+|[a-zA-Z0-9]+).*",
        Some("https://sukebei.nyaa.si/?f=0&c=0_0&q={q}"),
        Support::Ready,
        "",
    ),
    site(
        "NijiGazo",
        false,
        "https://niji-gazo.com",
        r"^https://niji-gazo\.com/view/\d+.*",
        None,
        Support::Cloudflare,
        "",
    ),
    SiteDef {
        clean: Clean::ReaderToDetails,
        format: Format::JsonCards("manga-cards"),
        ..site(
            "RokuHentai",
            false,
            "https://rokuhentai.com",
            r"^https://rokuhentai\.com/[a-z0-9]+(?:/\d+)?/?$",
            Some("https://rokuhentai.com/_search?q={q}"),
            Support::Ready,
            "the page is drawn by JavaScript from a JSON answer of the same search URL, which holds the cards as HTML; the pages of works are /id (details) and /id/0 (reader)",
        )
    },
    site(
        "HentaiEnvy",
        false,
        "https://hentaienvy.com",
        r"^https://hentaienvy\.com/[a-z]+/\d+.*",
        Some("https://hentaienvy.com/search/?key={q}"),
        Support::Ready,
        "",
    ),
    SiteDef {
        japanese: false,
        ..site(
            "HentaiEra",
            false,
            "https://hentaiera.com",
            r"^https://hentaiera\.com/[a-z]+/\d+.*",
            Some("https://hentaiera.com/search/?key={q}"),
            Support::Ready,
            "answers 快楽天 with nothing; the romaji finds the works",
        )
    },
    site("MasDr", false, "https://mas-dr.com", r"^https://mas-dr\.com/[a-z]+/\d+.*", None, Support::Closed, "does not answer"),
    site("JMwu", false, "https://m.jmwu.vip", r"^https://m\.jmwu\.vip/[a-z]+/\d+.*", None, Support::Closed, "HTTP 522"),
    site(
        "NukiBooks",
        false,
        "https://nukibooks.com",
        r"^https://nukibooks\.com/[a-z]+/\d+.*",
        Some("https://nukibooks.com/?s={q}"),
        Support::Cloudflare,
        "",
    ),
    SiteDef {
        delay_ms: 20_000,
        ..site(
            "177Picyy",
            false,
            "http://www.177picyy.com",
            r"^http://www\.177picyy\.com/[a-z]+/\d+.*",
            Some("http://www.177picyy.com/?s={q}"),
            Support::Ready,
            "answers 429 (Retry-After 10-20 s) to requests less than about 20 seconds apart, so it is asked that slowly",
        )
    },
    site("BatoTo", false, "https://bato.to", r"^https://bato\.to/[a-z]+/\d+.*", None, Support::Closed, "the site is closed"),
    site(
        "Wnacg",
        false,
        "https://www.wnacg.com",
        r"^https://www\.wnacg\.com/(?:[a-z]+/\d+|photos-index-aid-\d+\.html).*",
        Some("https://www.wnacg.com/search/?q={q}&f=_all&s=create_time_DESC&syn=yes"),
        Support::Ready,
        "the pages of albums are /photos-index-aid-N.html",
    ),
    site(
        "HentaiName",
        false,
        "https://www.hentai.name",
        r"^https://www\.hentai\.name/[a-z]+/\d+.*",
        None,
        Support::Cloudflare,
        "",
    ),
    site(
        "HentaiDesi",
        false,
        "http://jp.hentai.desi",
        r"^http://jp\.hentai\.desi/[a-z]+/\d+.*",
        None,
        Support::Closed,
        "does not answer",
    ),
    site(
        "AsmHentai",
        false,
        "https://asmhentai.com",
        r"^https://asmhentai\.com/[a-z]+/\d+.*",
        Some("https://asmhentai.com/search/?q={q}"),
        Support::Ready,
        "",
    ),
    site(
        "HentaiPaw",
        false,
        "https://hentaipaw.com",
        r"^https://hentaipaw\.com/[a-z]+/\d+.*",
        None,
        Support::Cloudflare,
        "",
    ),
    SiteDef {
        japanese: false,
        ..site(
            "EHentai",
            false,
            "https://e-hentai.org",
            r"^https://e-hentai\.org/[a-z]+/\d+.*",
            Some("https://e-hentai.org/?f_search={q}"),
            Support::Ready,
            "answers 快楽天 with nothing; the romaji finds the works",
        )
    },
];

/// A site with its patterns compiled.
pub struct Site {
    pub def: &'static SiteDef,
    pattern: Regex,
    filter: Option<Regex>,
    title: Title,
}

/// A result of a list page: all that is kept of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The path of the work's page, without the scheme and the domain (`/fanzine/mo4102658/`), and with
    /// the parameters of the query string that name the work.
    pub path: String,
    /// The title as the list page writes it.
    pub title: String,
}

impl Site {
    pub fn new(def: &'static SiteDef) -> Self {
        let build = |pattern: &str| {
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .unwrap_or_else(|e| panic!("the pattern of {} is wrong: {e}", def.name))
        };

        let title = TITLES.iter().find(|(name, _)| *name == def.name).map_or(Title::Text, |(_, title)| *title);

        Site { def, pattern: build(def.pattern), filter: def.filter.map(build), title }
    }

    /// The address of the search page for `query`. `None` for a site with no search URL.
    pub fn search_url(&self, query: &str) -> Option<String> {
        self.def.search.map(|template| {
            // {q+} writes a space as + (DLsite answers 403 to %20 in a path); {p+} does the same and puts a query without
            // Japanese in double quotes: DLsite finds the words in any place and lists the newest first, so an old issue
            // (COMIC E×E 11) is on page 3 of a plain search, and the first of a search for the phrase
            let phrase = if has_japanese(query) { query.to_string() } else { format!("\"{query}\"") };

            template
                .replace("{p+}", &percent_encode(&phrase).replace("%20", "+"))
                .replace("{p}", &percent_encode(&phrase))
                .replace("{q+}", &percent_encode(query).replace("%20", "+"))
                .replace("{q}", &percent_encode(query))
        })
    }

    /// The address of a work's page from its saved path.
    pub fn address(&self, path: &str) -> String {
        format!("{}{}", self.def.origin, path)
    }

    pub fn clean(&self, url: &str) -> String {
        let without_fragment = url.split('#').next().unwrap_or(url);

        match self.def.clean {
            Clean::Plain => without_fragment.to_string(),
            Clean::DeleteLocale => {
                static LOCALE: OnceLock<Regex> = OnceLock::new();
                let pattern = LOCALE.get_or_init(|| Regex::new(r"([?&])locale=[A-Za-z_]+&?").unwrap());

                pattern.replace_all(without_fragment, "$1").trim_end_matches(['?', '&']).to_string()
            }
            Clean::ViewToGallery => {
                static VIEW: OnceLock<Regex> = OnceLock::new();
                let pattern = VIEW.get_or_init(|| Regex::new(r"/view/(\d+)/\d+/?").unwrap());

                pattern.replace_all(without_fragment, "/gallery/$1/").to_string()
            }
            Clean::EpsToPage => {
                static EPS: OnceLock<Regex> = OnceLock::new();
                let pattern = EPS.get_or_init(|| Regex::new(r"/(?:page|eps)/([^/]+)/?\d*").unwrap());

                pattern.replace_all(without_fragment, "/page/$1").to_string()
            }
            Clean::ReaderToDetails => {
                static READER: OnceLock<Regex> = OnceLock::new();
                let pattern = READER.get_or_init(|| Regex::new(r"^(https://[^/]+/[^/]+)/\d+/?$").unwrap());

                pattern.replace(without_fragment, "$1").to_string()
            }
        }
    }

    /// The path that is saved for the address of a work: the path, and the parameters of the query that
    /// name the work. `None` for something that is not an address.
    pub fn path_of(&self, url: &str) -> Option<String> {
        let parsed = Url::parse(url).ok()?;
        let mut path = parsed.path().to_string();

        let kept: Vec<String> = parsed
            .query_pairs()
            .filter(|(name, _)| self.def.keep_query.contains(&name.as_ref()))
            .map(|(name, value)| format!("{name}={value}"))
            .collect();

        if !kept.is_empty() {
            path.push('?');
            path.push_str(&kept.join("&"));
        }

        Some(path)
    }

    /// The works of an answer of the search, whatever its format: the page, or the JSON that holds the cards.
    pub fn read(&self, page_url: &str, body: &str, limit: usize) -> Result<Vec<Link>, String> {
        match self.def.format {
            Format::Html => Ok(self.extract(page_url, body, limit)),
            Format::JsonCards(key) => {
                let json: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("the answer is not JSON: {e}"))?;
                let cards = json.get(key).and_then(|cards| cards.as_array()).ok_or_else(|| format!("the answer has no \"{key}\""))?;
                let html: String = cards.iter().filter_map(|card| card.as_str()).collect::<Vec<_>>().join("\n");

                Ok(self.extract(page_url, &html, limit))
            }
        }
    }

    /// The links of `html` (the page at `page_url`) that are results, without repeats, in the order of the
    /// page. A link without text gets the `title` attribute, or the `alt` of its image (the text of the
    /// image, not its address). A link that has no title at all is left out: it cannot be searched.
    pub fn extract(&self, page_url: &str, html: &str, limit: usize) -> Vec<Link> {
        static ANCHOR: OnceLock<Regex> = OnceLock::new();
        static HREF: OnceLock<Regex> = OnceLock::new();
        let anchor = ANCHOR.get_or_init(|| Regex::new(r"(?is)<a\b([^>]*)>(.*?)</a>").unwrap());
        let href = HREF.get_or_init(|| Regex::new(r#"(?is)\bhref\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());

        let Ok(base) = Url::parse(page_url) else { return Vec::new() };
        let mut links: Vec<Link> = Vec::new();

        for caps in anchor.captures_iter(html) {
            let attributes = &caps[1];
            let Some(target) = href.captures(attributes).and_then(|c| c.get(1).or(c.get(2))) else { continue };
            let Ok(absolute) = base.join(&decode(target.as_str())) else { continue };
            let absolute = absolute.to_string();

            if !self.pattern.is_match(&absolute) {
                continue;
            }

            let url = self.clean(&absolute);

            if self.filter.as_ref().is_some_and(|filter| !filter.is_match(&url)) {
                continue;
            }

            let Some(path) = self.path_of(&url) else { continue };
            let Some(title) = self.title_of(attributes, &caps[2]) else { continue };

            match links.iter_mut().find(|link| link.path == path) {
                // the same work is linked more than once: keep the longest title
                Some(known) => {
                    if title.chars().count() > known.title.chars().count() {
                        known.title = title;
                    }
                }
                None => links.push(Link { path, title }),
            }
        }

        links.truncate(limit);
        links
    }

    /// The title a link gives: the text of the link, or the part of its card that the site keeps the title in.
    /// None when the link does not carry the title (the picture of a card, its price).
    fn title_of(&self, attributes: &str, inner: &str) -> Option<String> {
        let title = match self.title {
            Title::Text => link_title(attributes, inner),
            Title::ImgAlt => image_alt(inner),
            Title::Tag(tag) => element_text(inner, |name, _| name == tag),
            Title::Class(class) => element_text(inner, |_, classes| classes.split_whitespace().any(|c| c == class)),
            Title::Bookmark if attributes.contains("rel=\"bookmark\"") => text_of(inner),
            Title::Bookmark => String::new(),
        };

        (!title.is_empty()).then_some(title)
    }
}

pub fn all() -> Vec<Site> {
    SITES.iter().map(Site::new).collect()
}

/// The text of a link as the page writes it; the `title` attribute or the `alt` of an image when the link
/// has no text.
fn link_title(attributes: &str, inner: &str) -> String {
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static TITLE: OnceLock<Regex> = OnceLock::new();
    static ALT: OnceLock<Regex> = OnceLock::new();
    let tags = TAGS.get_or_init(|| Regex::new(r"(?is)<[^>]*>").unwrap());
    let title = TITLE.get_or_init(|| Regex::new(r#"(?is)\btitle\s*=\s*"([^"]*)""#).unwrap());
    let alt = ALT.get_or_init(|| Regex::new(r#"(?is)<img\b[^>]*\balt\s*=\s*"([^"]*)""#).unwrap());

    let text = squash(&decode(&tags.replace_all(inner, " ")));
    if !text.is_empty() {
        return text;
    }

    let from_attribute = title.captures(attributes).map(|c| squash(&decode(&c[1]))).unwrap_or_default();
    if !from_attribute.is_empty() {
        return from_attribute;
    }

    alt.captures(inner).map(|c| squash(&decode(&c[1]))).unwrap_or_default()
}

/// The text of some HTML as a page shows it.
fn text_of(html: &str) -> String {
    static TAGS: OnceLock<Regex> = OnceLock::new();
    let tags = TAGS.get_or_init(|| Regex::new(r"(?is)<[^>]*>").unwrap());

    squash(&decode(&tags.replace_all(html, " ")))
}

/// The alt of the first image of some HTML.
fn image_alt(html: &str) -> String {
    static ALT: OnceLock<Regex> = OnceLock::new();
    let alt = ALT.get_or_init(|| Regex::new(r#"(?is)<img\b[^>]*\balt\s*=\s*"([^"]*)""#).unwrap());

    alt.captures(html).map(|c| squash(&decode(&c[1]))).unwrap_or_default()
}

/// The text of the first element of the HTML that the test accepts. The test gets the tag name (lower case)
/// and the class attribute. The element ends at the first closing tag of its name (a title has no element of
/// the same name in it).
fn element_text(html: &str, accepts: impl Fn(&str, &str) -> bool) -> String {
    static OPEN: OnceLock<Regex> = OnceLock::new();
    static CLASS: OnceLock<Regex> = OnceLock::new();
    let open = OPEN.get_or_init(|| Regex::new(r"(?is)<([a-z][a-z0-9]*)\b([^>]*)>").unwrap());
    let class = CLASS.get_or_init(|| Regex::new(r#"(?is)\bclass\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());

    for caps in open.captures_iter(html) {
        let tag = caps[1].to_ascii_lowercase();
        let classes = class
            .captures(&caps[2])
            .and_then(|c| c.get(1).or(c.get(2)))
            .map_or("", |m| m.as_str());

        if !accepts(&tag, classes) {
            continue;
        }

        let rest = &html[caps.get(0).map_or(0, |m| m.end())..];
        let end = rest.to_ascii_lowercase().find(&format!("</{tag}")).unwrap_or(rest.len());
        let text = text_of(&rest[..end]);

        if !text.is_empty() {
            return text;
        }
    }

    String::new()
}

/// A page shows runs of white space (the indentation of the HTML, a line break) as one space.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Character references (`&amp;` `&#39;` `&#x3042;`) to characters.
pub fn decode(text: &str) -> String {
    static REFERENCE: OnceLock<Regex> = OnceLock::new();
    let reference =
        REFERENCE.get_or_init(|| Regex::new(r"(?i)&(?:#(\d+)|#x([0-9a-f]+)|(\w+));").unwrap());

    reference
        .replace_all(text, |caps: &regex::Captures| {
            let code = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .or_else(|| caps.get(2).and_then(|m| u32::from_str_radix(m.as_str(), 16).ok()));

            if let Some(c) = code.and_then(char::from_u32) {
                return c.to_string();
            }

            match caps.get(3).map(|m| m.as_str().to_ascii_lowercase()).as_deref() {
                Some("amp") => "&".to_string(),
                Some("lt") => "<".to_string(),
                Some("gt") => ">".to_string(),
                Some("quot") => "\"".to_string(),
                Some("apos") => "'".to_string(),
                Some("nbsp") => " ".to_string(),
                _ => caps[0].to_string(),
            }
        })
        .to_string()
}

/// Percent-encodes everything but the unreserved characters, so it is safe in a path and in a query.
pub fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len() * 3);

    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }

    encoded
}

/// Does the page look like a Cloudflare challenge instead of the site?
pub fn is_challenge(status: u16, html: &str) -> bool {
    matches!(status, 403 | 429 | 503)
        && (html.contains("Just a moment") || html.contains("cf-chl") || html.contains("Attention Required"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(name: &str) -> Site {
        Site::new(SITES.iter().find(|s| s.name == name).unwrap())
    }

    fn link(path: &str, title: &str) -> Link {
        Link { path: path.into(), title: title.into() }
    }

    #[test]
    fn every_pattern_compiles_and_every_ready_site_has_a_search_url() {
        for def in SITES {
            Site::new(def);

            assert!(def.origin.starts_with("http"), "{} has no origin", def.name);
            if def.support == Support::Ready {
                assert!(def.search.is_some_and(|s| s.contains("{q") || s.contains("{p")), "{} has no search URL", def.name);
            }
        }
        assert_eq!(SITES.len(), 28, "websites.ts lists 28 sites");
    }

    #[test]
    fn the_query_is_percent_encoded_for_paths_and_queries() {
        assert_eq!(percent_encode("COMIC ExE 11"), "COMIC%20ExE%2011");
        assert_eq!(percent_encode("快楽天"), "%E5%BF%AB%E6%A5%BD%E5%A4%A9");
        assert_eq!(percent_encode("a&b=c/d"), "a%26b%3Dc%2Fd");
        assert_eq!(
            site("momon:GA").search_url("快楽天").unwrap(),
            "https://momon-ga.com/?s=%E5%BF%AB%E6%A5%BD%E5%A4%A9"
        );
        assert!(site("MasDr").search_url("x").is_none());
        // DLsite refuses %20 in the path: a space is +
        assert!(site("DLsite").search_url("快楽天 2026").unwrap().contains("keyword/%E5%BF%AB%E6%A5%BD%E5%A4%A9+2026/"));
        // ... and a query without Japanese is a phrase (the old issues are far down a plain search, newest first)
        assert!(site("DLsite").search_url("COMIC E×E 11").unwrap().contains("keyword/%22COMIC+E%C3%97E+11%22/"));
        assert!(site("FANZA").search_url("COMIC E×E 11").unwrap().ends_with("searchstr=%22COMIC%20E%C3%97E%2011%22/"));
        assert!(site("FANZA").search_url("快楽天 2026").unwrap().ends_with("searchstr=%E5%BF%AB%E6%A5%BD%E5%A4%A9%202026/"));
    }

    #[test]
    fn only_the_path_and_the_title_are_kept() {
        let html = r#"
            <a href="/gallery/4102658/"><img src="https://img.example/cover.jpg" alt="COMIC 快楽天 2026年11月号"></a>
            <a href="/gallery/3767154/#comments">Second &amp; third</a>
            <a href="/gallery/3767154/" title="ignored: the text wins">Second &amp; third (long)</a>
            <a href="/genre/">Genre</a>
            <a href='https://hentaienvy.com/gallery/99/?utm=x'>  one
              two </a>
            <a href="https://other.example/gallery/1/">elsewhere</a>
            <a href="/gallery/5/"><img src="x.jpg"></a>
        "#;
        let links = site("HentaiEnvy").extract("https://hentaienvy.com/search/?key=x", html, 50);

        assert_eq!(
            links,
            vec![
                link("/gallery/4102658/", "COMIC 快楽天 2026年11月号"),
                // the same work, linked twice: one result with the longer title
                link("/gallery/3767154/", "Second & third (long)"),
                // the title as the page writes it; the query string is not part of the path
                link("/gallery/99/", "one two"),
            ],
            "no image address anywhere, and a link without any title (gallery 5) is left out"
        );
        assert!(links.iter().all(|l| !l.path.contains("http") && !l.path.contains("jpg")));
    }

    #[test]
    fn the_address_is_put_back_from_the_origin_and_the_path() {
        assert_eq!(site("momon:GA").address("/fanzine/mo1/"), "https://momon-ga.com/fanzine/mo1/");
        assert_eq!(
            site("同人の森").address("/comics/detail?uuid=abc"),
            "https://doujinnomori.com/comics/detail?uuid=abc"
        );
    }

    #[test]
    fn a_query_parameter_that_names_the_work_is_kept_and_the_others_are_not() {
        let html = r#"<a href="/comics/detail?uuid=c77a6aba-6b02-4b5d-855e-55b5f222fd37&type=prev">Title</a>"#;
        let links = site("同人の森").extract("https://doujinnomori.com/comics/free-search?keyword=x", html, 50);

        assert_eq!(links, vec![link("/comics/detail?uuid=c77a6aba-6b02-4b5d-855e-55b5f222fd37", "Title")]);
    }

    #[test]
    fn addresses_are_tidied_up_per_site() {
        assert_eq!(
            site("DLsite").clean("https://www.dlsite.com/books/work/=/product_id/BJ1.html?locale=ja_JP"),
            "https://www.dlsite.com/books/work/=/product_id/BJ1.html"
        );
        assert_eq!(
            site("DLsite").clean("https://www.dlsite.com/books/work/=/product_id/BJ1.html?a=1&locale=ja_JP&b=2"),
            "https://www.dlsite.com/books/work/=/product_id/BJ1.html?a=1&b=2"
        );
        assert_eq!(site("IMHentai").clean("https://imhentai.xxx/view/477585/3/"), "https://imhentai.xxx/gallery/477585/");
        assert_eq!(site("JComic").clean("https://jcomic.net/eps/abc/12"), "https://jcomic.net/page/abc");
        assert_eq!(site("R18Best").clean("https://r18.best/826249#more-826249"), "https://r18.best/826249");
    }

    #[test]
    fn fanza_counts_only_product_pages_of_the_book_store() {
        let html = r#"
            <a href="https://www.dmm.co.jp/service/-/exchange/=/?url=x"><p class="line-clamp-2">nav</p></a>
            <a href="https://book.dmm.co.jp/product/25199/b915awnmg04607/?i3_ref=search&amp;i3_ord=1"><p class="x line-clamp-2">COMIC快楽天</p></a>
        "#;
        let links = site("FANZA").extract("https://www.dmm.co.jp/search/=/searchstr=x/", html, 50);

        assert_eq!(links, vec![link("/product/25199/b915awnmg04607/", "COMIC快楽天")], "the tracking parameters are dropped");
    }

    #[test]
    fn nya_hentai_is_found_at_either_address_with_the_same_path() {
        let html = r#"<a href="https://nyahentai.one/magazine/re3280150/"><span>a</span></a><a href="https://nyahentai.re/fanzine/re1/"><span>b</span></a>"#;
        let links = site("Nya:Hentai").extract("https://nyahentai.one/?s=x", html, 50);

        assert_eq!(links, vec![link("/magazine/re3280150/", "a"), link("/fanzine/re1/", "b")]);
    }

    /// The cards of the list pages, as the sites write them (the badge, the price and the date are not the title).
    #[test]
    fn the_title_is_read_from_the_part_of_the_card_that_holds_it() {
        // momon:GA: a badge of likes, and the title in the image alt and in a span
        let momon = r#"<a href="https://momon-ga.com/fanzine/mo3813165/"> <div class="post-list-image"> <img src="https://z3.momon-ga.me/1.webp" alt="はたちのつどい COMIC 快楽天 2026年3月号" /> <div class="post-list-wpulike">58+</div> </div> <span>はたちのつどい COMIC 快楽天 2026年3月号</span> </a>"#;
        assert_eq!(
            site("momon:GA").extract("https://momon-ga.com/?s=x", momon, 50),
            vec![link("/fanzine/mo3813165/", "はたちのつどい COMIC 快楽天 2026年3月号")]
        );

        // Nya:Hentai: the date of the post follows the title
        let nya = r#"<a href="https://nyahentai.one/magazine/re3280150/"> <div class="post-list-image"><img src="x.webp"></div> <span>彼女にできること (COMIC 快楽天 2018年8月号)</span> <div class="post-list-time">2026年04月30日</div> </a>"#;
        assert_eq!(
            site("Nya:Hentai").extract("https://nyahentai.one/?s=x", nya, 50),
            vec![link("/magazine/re3280150/", "彼女にできること (COMIC 快楽天 2018年8月号)")]
        );

        // JComic: the author follows the title, in the same card
        let jcomic = r#"<a target="_blank" href="/page/abc"> <div><img class="img-responsive comic-thumb" src="https://images.jcomic.net/1.jpg" alt=""></div> <div> <p class='comic-title'>我が光 (COMIC 快楽天 2026年11月号) (24)</p> <p class='comic-author'>作者:</p><a href="/author/x"><button>どろずみ</button></a>"#;
        assert_eq!(
            site("JComic").extract("https://jcomic.net/search/x", jcomic, 50),
            vec![link("/page/abc", "我が光 (COMIC 快楽天 2026年11月号) (24)")]
        );

        // R18Best: the "Read More" link and the title link go to the same page
        let r18 = r#"<a href="https://r18.best/826122">Read More &amp;ldquo;[雑誌] WEEKLY快楽天 2026 No.40&amp;rdquo; &raquo;</a><a href="https://r18.best/826122" rel="bookmark">[雑誌] WEEKLY快楽天 2026 No.40</a>"#;
        assert_eq!(
            site("R18Best").extract("https://r18.best/?s=x", r18, 50),
            vec![link("/826122", "[雑誌] WEEKLY快楽天 2026 No.40")]
        );

        // FANZA: the card is several links to the same page; only the one with the title counts
        let fanza = r#"
            <a href="https://book.dmm.co.jp/product/1/a/"><img alt="Product" src="https://ebook-assets.dmm.co.jp/a.jpg"/></a>
            <a href="https://book.dmm.co.jp/product/1/a/"><p class="text-[#b42f59]"><span>990円</span></p><p class="x">作家：誰か</p></a>
            <a href="https://book.dmm.co.jp/product/1/a/"><p class="text-sm font-bold line-clamp-2">COMIC快楽天 2026年11月号</p></a>
        "#;
        assert_eq!(
            site("FANZA").extract("https://www.dmm.co.jp/search/=/searchstr=x/", fanza, 50),
            vec![link("/product/1/a/", "COMIC快楽天 2026年11月号")]
        );
    }

    #[test]
    fn rokuhentai_cards_come_in_json_and_the_reader_is_the_details_page() {
        let card = r#"<div class="mdc-card"><a class="site-popunder-ad-slot" href="https://rokuhentai.com/nfddg5/0"><div class="mdc-card__media" style="background-image: url(&quot;https://rokuhentai.com/_images/cover-thumbnails/nfddg5.jpg&quot;);"></div><div class="site-manga-card__title"><div class="mdc-typography--body1 site-manga-card__title--primary">🇯🇵 [火鳥] 異世快楽天救済秘話 (異世快楽天 Vol.10)</div><div class="mdc-typography--caption">6 images</div></div></a><a class="x" href="https://rokuhentai.com/nfddg5"><span>Details</span></a><a href="https://rokuhentai.com/tag/foo">tag</a></div>"#;
        let body = serde_json::json!({ "manga-ids": ["nfddg5"], "manga-cards": [card], "prev": null }).to_string();
        let roku = site("RokuHentai");

        assert_eq!(
            roku.read("https://rokuhentai.com/_search?q=x", &body, 50).unwrap(),
            vec![link("/nfddg5", "🇯🇵 [火鳥] 異世快楽天救済秘話 (異世快楽天 Vol.10)")],
            "the reader and the details are one work, and the cover is not saved"
        );
        assert!(roku.read("https://rokuhentai.com/_search?q=x", "<html>", 50).is_err());
        assert!(roku.read("https://rokuhentai.com/_search?q=x", "{}", 50).is_err());
        assert_eq!(roku.clean("https://rokuhentai.com/nfddg5/0"), "https://rokuhentai.com/nfddg5");
        assert_eq!(roku.clean("https://rokuhentai.com/nfddg5"), "https://rokuhentai.com/nfddg5");
    }

    #[test]
    fn only_the_sites_that_find_japanese_titles_are_asked_in_japanese() {
        let romaji_only: Vec<_> = SITES.iter().filter(|s| !s.japanese).map(|s| s.name).collect();

        assert_eq!(romaji_only, ["HentaiEra", "EHentai"]);
    }

    #[test]
    fn the_limit_cuts_the_results() {
        let html: String = (1..=10).map(|n| format!(r#"<a href="/gallery/{n}/">w{n}</a>"#)).collect();

        assert_eq!(site("HentaiEnvy").extract("https://hentaienvy.com/", &html, 3).len(), 3);
    }

    #[test]
    fn a_challenge_page_is_told_from_a_page() {
        assert!(is_challenge(403, "<title>Just a moment...</title>"));
        assert!(is_challenge(503, "x cf-chl-opt y"));
        assert!(!is_challenge(200, "Just a moment"), "a normal page that says so is not a challenge");
        assert!(!is_challenge(403, "<title>Forbidden</title>"));
    }

    #[test]
    fn character_references_are_decoded() {
        assert_eq!(decode("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x3042;"), "a & b <c> \"d\" 'e' あ");
        assert_eq!(decode("&unknown; stays"), "&unknown; stays");
    }
}
