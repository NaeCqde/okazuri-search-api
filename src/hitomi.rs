//! The search of Hitomi.La. The site has no page that lists the results: its JavaScript reads static files on
//! `ltn.gold-usergeneratedcontent.net`, and so does this.
//!
//! - `/galleriesindex/version` is the version of the index (a number).
//! - `/galleriesindex/galleries.<version>.index` is a B-tree (B = 16) of the words of the titles. A key is the first 4 bytes of the
//!   SHA-256 of a word (the word in lower case, as written: Japanese words are in it too, and so are the words of the
//!   English title). A node is read with a Range request of 464 bytes from its address; the root is at 0. The
//!   data of a key is an offset and a length in `/galleriesindex/galleries.<version>.data`: a count, then the
//!   ids of the galleries that have the word in their title (big-endian, 4 bytes each, the newest first).
//! - `/galleries/<id>.js` is `var galleryinfo = {...}`: the title (English, or the romaji) and the Japanese title.
//!
//! The ids of the words of a query are intersected (the shortest list first), the newest `limit` are kept, and the title of each is read. A
//! word that is in no title is left out, so a romaji that spells a word in another way (`shi tara` for `shitara`) does not
//! empty the answer; the engine matches every word with the saved title in the end.
//!
//! The work is kept as `/galleries/<id>.html` (the site redirects it to the page of the gallery) with the title
//! `<title> | <Japanese title>`: a query in Japanese, in romaji and in English all find it.

use crate::sites::Link;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

/// The host of the index files.
pub const ORIGIN: &str = "https://ltn.gold-usergeneratedcontent.net";

const B: usize = 16;
const MAX_NODE_SIZE: u64 = 464;
const MAX_KEY_SIZE: usize = 32;
/// The most words of a query that are looked up.
const MAX_WORDS: usize = 6;
/// A word that is in more titles than this (4 bytes each) is left out: it narrows nothing, and costs a long answer.
const MAX_LIST_BYTES: i64 = 2_000_000;
/// The deepest tree that is followed (a tree of this site has 6 or 7 levels).
const MAX_DEPTH: usize = 24;

/// Reads a path of the index host: the whole answer, or the bytes of the range (both ends included).
#[allow(async_fn_in_trait)]
pub trait Fetch {
    async fn get(&mut self, path: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>, String>;
}

/// The key of a word: the first 4 bytes of its SHA-256.
pub fn hash_word(word: &str) -> [u8; 4] {
    let digest = Sha256::digest(word.as_bytes());

    [digest[0], digest[1], digest[2], digest[3]]
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Node {
    keys: Vec<Vec<u8>>,
    datas: Vec<(u64, i32)>,
    subnodes: Vec<u64>,
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or("the node is cut short")?;
        let part = &self.bytes[self.at..end];

        self.at = end;
        Ok(part)
    }

    fn int(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn long(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
}

fn decode_node(bytes: &[u8]) -> Result<Node, String> {
    let mut reader = Reader { bytes, at: 0 };
    let mut node = Node::default();

    let keys = reader.int()?;
    for _ in 0..keys.clamp(0, 2 * B as i32) {
        let size = reader.int()?;
        if size <= 0 || size as usize > MAX_KEY_SIZE {
            return Err(format!("a key of {size} bytes"));
        }
        node.keys.push(reader.take(size as usize)?.to_vec());
    }

    let datas = reader.int()?;
    for _ in 0..datas.clamp(0, 2 * B as i32) {
        let offset = reader.long()?;
        let length = reader.int()?;
        node.datas.push((offset, length));
    }

    for _ in 0..=B {
        node.subnodes.push(reader.long()?);
    }

    Ok(node)
}

/// The place of `key` in the node: whether it is there, and the index of the first key that is not smaller (the index of
/// the subnode to go on in, when it is not there). The keys are compared byte by byte as far as the shorter one goes.
fn locate(key: &[u8], node: &Node) -> (bool, usize) {
    for (index, other) in node.keys.iter().enumerate() {
        let length = key.len().min(other.len());

        match key[..length].cmp(&other[..length]) {
            std::cmp::Ordering::Less => return (false, index),
            std::cmp::Ordering::Equal => return (true, index),
            std::cmp::Ordering::Greater => {}
        }
    }

    (false, node.keys.len())
}

/// The ids of a list of the data file.
fn decode_ids(bytes: &[u8]) -> Result<Vec<u32>, String> {
    let mut reader = Reader { bytes, at: 0 };
    let count = reader.int()?;

    if count <= 0 || bytes.len() != 4 + 4 * count as usize {
        return Err(format!("a list of {count} ids in {} bytes", bytes.len()));
    }

    Ok(bytes[4..].chunks_exact(4).map(|id| u32::from_be_bytes(id.try_into().unwrap())).collect())
}

/// Where the list of a word is in the data file: its offset and its length. None when no title has the word.
async fn find(fetch: &mut impl Fetch, index: &str, word: &str) -> Result<Option<(u64, i32)>, String> {
    let key = hash_word(word);
    let mut address = 0u64;

    for _ in 0..MAX_DEPTH {
        let node = decode_node(&fetch.get(index, Some((address, address + MAX_NODE_SIZE - 1))).await?)?;

        if node.keys.is_empty() {
            return Ok(None);
        }

        let (there, at) = locate(&key, &node);
        if there {
            return Ok(node.datas.get(at).copied());
        }

        // a leaf has no subnode
        match node.subnodes.get(at) {
            Some(next) if *next != 0 && node.subnodes.iter().any(|address| *address != 0) => address = *next,
            _ => return Ok(None),
        }
    }

    Err("the index is deeper than it can be".to_string())
}

/// The ids of the galleries that have every word (the ones that are in the index) in their title, the newest first, as
/// many as `limit`.
pub async fn gallery_ids(fetch: &mut impl Fetch, query: &str, limit: usize) -> Result<Vec<u32>, String> {
    let version = String::from_utf8_lossy(&fetch.get("/galleriesindex/version", None).await?).trim().to_string();
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("the version of the index is {version:?}"));
    }
    let index = format!("/galleriesindex/galleries.{version}.index");
    let data = format!("/galleriesindex/galleries.{version}.data");

    let lowered = query.to_lowercase();
    let mut words: Vec<&str> = Vec::new();
    for word in lowered.split_whitespace() {
        if !words.contains(&word) {
            words.push(word);
        }
    }
    words.truncate(MAX_WORDS);

    // where the lists are: the shortest list first
    let mut lists: Vec<(u64, i32)> = Vec::new();
    for word in words {
        if let Some(list) = find(fetch, &index, word).await? {
            if list.1 > 4 && i64::from(list.1) <= MAX_LIST_BYTES {
                lists.push(list);
            }
        }
    }
    lists.sort_by_key(|(_, length)| *length);

    let mut ids: Option<Vec<u32>> = None;

    for (offset, length) in lists {
        let list = decode_ids(&fetch.get(&data, Some((offset, offset + length as u64 - 1))).await?)?;

        ids = Some(match ids {
            None => list,
            Some(known) => {
                let keep: HashSet<u32> = list.into_iter().collect();

                known.into_iter().filter(|id| keep.contains(id)).collect()
            }
        });

        // nothing is left: the other words cannot bring anything back
        if ids.as_ref().is_some_and(Vec::is_empty) {
            break;
        }
    }

    let mut ids = ids.unwrap_or_default();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids.truncate(limit);

    Ok(ids)
}

/// What `/galleries/<id>.js` says of a gallery: its title (English, or the romaji, or both when the language is not
/// Japanese) and its Japanese title.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gallery {
    pub id: u32,
    pub title: String,
    pub japanese_title: Option<String>,
    /// The language of the gallery: `japanese`, `english`, `chinese`... (none when the site does not say).
    pub language: Option<String>,
    /// `doujinshi`, `manga`... (none when the site does not say).
    pub kind: Option<String>,
    /// The artists, the groups and the parodies (the series), in lower case, as the site names them.
    pub artists: Vec<String>,
    pub groups: Vec<String>,
    pub parodies: Vec<String>,
    /// The number of pages.
    pub pages: usize,
}

impl Gallery {
    /// Whether the two are the same work in two languages: the same artists, groups, parodies and kind, and about the same
    /// number of pages (a translation can add a page of credits). A gallery with neither an artist nor a group is never the same.
    pub fn same_work(&self, other: &Gallery) -> bool {
        let set = |names: &[String]| names.iter().cloned().collect::<HashSet<String>>();

        (!self.artists.is_empty() || !self.groups.is_empty())
            && set(&self.artists) == set(&other.artists)
            && set(&self.groups) == set(&other.groups)
            && set(&self.parodies) == set(&other.parodies)
            && self.kind == other.kind
            && self.pages.abs_diff(other.pages) <= 3
    }

    /// The path of the page of the gallery (the site redirects it to the page with the title in its address).
    pub fn path(&self) -> String {
        format!("/galleries/{}.html", self.id)
    }

    /// The English title of a gallery in English: `English | romaji` is the English before the bar.
    pub fn english_title(&self) -> String {
        use crate::engine::Lang;

        match self.title.rsplit_once(" | ") {
            Some((head, tail)) if Lang::of(head) == Some(Lang::English) && Lang::of(tail) == Some(Lang::English) => head.trim().to_string(),
            _ => self.title.trim().to_string(),
        }
    }

    /// The work as it is saved: the title, and the Japanese title after a bar (not written twice when the title has it).
    pub fn link(&self) -> Link {
        let title = match &self.japanese_title {
            Some(japanese) if !self.title.contains(japanese.as_str()) => format!("{} | {japanese}", self.title),
            _ => self.title.clone(),
        };

        Link { path: self.path(), title }
    }
}

/// The gallery that `/galleries/<id>.js` describes. None when it has no title.
pub fn parse_gallery(id: u32, body: &str) -> Option<Gallery> {
    let json = body.trim().strip_prefix("var galleryinfo")?.trim_start().strip_prefix('=')?.trim().trim_end_matches(';');
    let info: serde_json::Value = serde_json::from_str(json).ok()?;

    let text = |key: &str| info.get(key).and_then(|value| value.as_str()).map(str::trim).filter(|text| !text.is_empty());

    // the names of a list like `"artists": [{"artist": "kujiran", "url": "..."}]`
    let names = |list: &str, key: &str| -> Vec<String> {
        info.get(list)
            .and_then(|value| value.as_array())
            .map(|items| items.iter().filter_map(|item| item.get(key)?.as_str()).map(|name| name.trim().to_lowercase()).collect())
            .unwrap_or_default()
    };

    Some(Gallery {
        id,
        title: text("title")?.to_string(),
        japanese_title: text("japanese_title").map(str::to_string),
        language: text("language").map(str::to_lowercase),
        kind: text("type").map(str::to_lowercase),
        artists: names("artists", "artist"),
        groups: names("groups", "group"),
        parodies: names("parodys", "parody"),
        pages: info.get("files").and_then(|files| files.as_array()).map_or(0, Vec::len),
    })
}

/// The galleries for a query: at most `limit`, the newest first. A gallery whose file is gone is left out; when none of
/// them can be read, that is an error.
pub async fn galleries(fetch: &mut impl Fetch, query: &str, limit: usize) -> Result<Vec<Gallery>, String> {
    let ids = gallery_ids(fetch, query, limit).await?;
    let mut found = Vec::new();
    let mut failed = None;

    for id in &ids {
        match fetch.get(&format!("/galleries/{id}.js"), None).await {
            Ok(body) => found.extend(parse_gallery(*id, &String::from_utf8_lossy(&body))),
            Err(e) => failed = Some(e),
        }
    }

    match failed {
        Some(error) if found.is_empty() && !ids.is_empty() => Err(error),
        _ => Ok(found),
    }
}

/// The works for a query: at most `limit`, the newest first.
pub async fn search(fetch: &mut impl Fetch, query: &str, limit: usize) -> Result<Vec<Link>, String> {
    Ok(galleries(fetch, query, limit).await?.iter().map(Gallery::link).collect())
}

/// Letters and digits in lower case, without the spaces and the marks between them: how a title is compared with the text.
pub fn squash(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn node(keys: &[[u8; 4]], datas: &[(u64, i32)], subnodes: &[u64]) -> Vec<u8> {
        let mut bytes = Vec::new();

        bytes.extend((keys.len() as i32).to_be_bytes());
        for key in keys {
            bytes.extend(4i32.to_be_bytes());
            bytes.extend(key);
        }
        bytes.extend((datas.len() as i32).to_be_bytes());
        for (offset, length) in datas {
            bytes.extend(offset.to_be_bytes());
            bytes.extend(length.to_be_bytes());
        }
        for index in 0..=B {
            bytes.extend(subnodes.get(index).copied().unwrap_or(0).to_be_bytes());
        }

        bytes
    }

    fn ids(list: &[u32]) -> Vec<u8> {
        let mut bytes = (list.len() as i32).to_be_bytes().to_vec();

        for id in list {
            bytes.extend(id.to_be_bytes());
        }

        bytes
    }

    /// A site in memory: the words, each with its list of ids, in a tree of two levels (a root with two leaves), and the
    /// galleries.
    struct Site {
        files: HashMap<String, Vec<u8>>,
        requests: usize,
    }

    impl Site {
        fn new(words: &[(&str, &[u32])], galleries: &[(u32, &str)]) -> Self {
            let mut sorted: Vec<([u8; 4], &[u32])> = words.iter().map(|(word, list)| (hash_word(word), *list)).collect();
            sorted.sort();

            let mut data = Vec::new();
            let mut places = Vec::new();
            for (_, list) in &sorted {
                let bytes = ids(list);

                places.push((data.len() as u64, bytes.len() as i32));
                data.extend(bytes);
            }

            // the middle word is the root, the others are the leaves on its two sides (each leaf is padded to the size of a node)
            let pad = |mut bytes: Vec<u8>| {
                bytes.resize(MAX_NODE_SIZE as usize, 0);
                bytes
            };
            let middle = sorted.len() / 2;
            let left = pad(node(&sorted[..middle].iter().map(|(k, _)| *k).collect::<Vec<_>>(), &places[..middle], &[]));
            let right = pad(node(&sorted[middle + 1..].iter().map(|(k, _)| *k).collect::<Vec<_>>(), &places[middle + 1..], &[]));
            let root = pad(node(&[sorted[middle].0], &[places[middle]], &[MAX_NODE_SIZE, 2 * MAX_NODE_SIZE]));

            let mut index = root;
            index.extend(left);
            index.extend(right);

            let mut files = HashMap::new();
            files.insert("/galleriesindex/version".to_string(), b"1795000020\n".to_vec());
            files.insert("/galleriesindex/galleries.1795000020.index".to_string(), index);
            files.insert("/galleriesindex/galleries.1795000020.data".to_string(), data);
            for (id, title) in galleries {
                files.insert(format!("/galleries/{id}.js"), format!("var galleryinfo = {title};").into_bytes());
            }

            Site { files, requests: 0 }
        }

    }

    impl Fetch for Site {
        async fn get(&mut self, path: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>, String> {
            self.requests += 1;
            let file = self.files.get(path).ok_or_else(|| format!("HTTP 404 {path}"))?;

            Ok(match range {
                Some((from, to)) => file[(from as usize).min(file.len())..(to as usize + 1).min(file.len())].to_vec(),
                None => file.clone(),
            })
        }
    }

    fn site() -> Site {
        Site::new(
            &[
                ("kyoudai", &[30, 20, 10]),
                ("ikemasen", &[30, 25, 10]),
                ("姉弟", &[30, 5]),
                ("comic", &[40, 30, 25, 20, 10]),
                ("de", &[50, 40, 30, 25, 20, 10, 5]),
            ],
            &[
                (30, r#"{"id":"30","title":"Kyoudai de Shitara Ikemasen","japanese_title":"姉弟でシたらいけません"}"#),
                (10, r#"{"id":"10","title":"Kyoudai Comic","japanese_title":null}"#),
                (5, r#"{"id":"5","title":"Ane to Otouto | 姉弟","japanese_title":"姉弟"}"#),
            ],
        )
    }

    #[test]
    fn the_hash_of_a_word_is_the_first_four_bytes_of_its_sha256() {
        // sha256("abc") = ba7816bf 8f01cfea ...
        assert_eq!(hash_word("abc"), [0xba, 0x78, 0x16, 0xbf]);
    }

    #[test]
    fn a_node_is_read_and_the_key_is_located() {
        let bytes = node(&[[1, 0, 0, 0], [5, 0, 0, 0]], &[(0, 8), (8, 12)], &[0, 464, 928]);
        let node = decode_node(&bytes).unwrap();

        assert_eq!(node.keys.len(), 2);
        assert_eq!(node.datas, [(0, 8), (8, 12)]);
        assert_eq!(node.subnodes.len(), B + 1);
        assert_eq!(locate(&[5, 0, 0, 0], &node), (true, 1));
        assert_eq!(locate(&[3, 0, 0, 0], &node), (false, 1), "between the keys: the subnode in the middle");
        assert_eq!(locate(&[9, 0, 0, 0], &node), (false, 2), "after the last key");
        assert!(decode_node(&bytes[..10]).is_err(), "a node that is cut short is an error, not a panic");
    }

    #[test]
    fn a_list_of_ids_must_be_as_long_as_it_says() {
        assert_eq!(decode_ids(&ids(&[3, 2, 1])).unwrap(), [3, 2, 1]);
        assert!(decode_ids(&ids(&[3, 2, 1])[..10]).is_err());
        assert!(decode_ids(&[0, 0, 0, 0]).is_err());
    }

    #[tokio::test]
    async fn the_ids_of_a_word_are_found_in_a_tree_of_two_levels() {
        let mut site = site();

        for (word, count) in [("kyoudai", 3), ("ikemasen", 3), ("姉弟", 2), ("comic", 5), ("de", 7)] {
            assert_eq!(gallery_ids(&mut site, word, 100).await.unwrap().len(), count, "{word}");
        }
        assert!(gallery_ids(&mut site, "nothere", 100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_words_are_intersected_the_newest_first_and_an_unknown_word_is_left_out() {
        let mut site = site();

        assert_eq!(gallery_ids(&mut site, "Kyoudai Ikemasen", 100).await.unwrap(), [30, 10]);
        assert_eq!(gallery_ids(&mut site, "kyoudai de shi ikemasen", 100).await.unwrap(), [30, 10], "`shi` is in no title");
        assert_eq!(gallery_ids(&mut site, "comic", 2).await.unwrap(), [40, 30], "the newest ones");
        assert!(gallery_ids(&mut site, "shi", 100).await.unwrap().is_empty());
        assert!(gallery_ids(&mut site, "", 100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_search_reads_the_titles_of_the_galleries_and_skips_a_gallery_that_is_gone() {
        let mut site = site();

        // 20 has a list entry and no file
        assert_eq!(
            search(&mut site, "kyoudai", 10).await.unwrap(),
            [
                Link { path: "/galleries/30.html".into(), title: "Kyoudai de Shitara Ikemasen | 姉弟でシたらいけません".into() },
                Link { path: "/galleries/10.html".into(), title: "Kyoudai Comic".into() },
            ]
        );
    }

    #[test]
    fn a_gallery_without_a_title_is_left_out_and_the_japanese_title_is_not_written_twice() {
        assert_eq!(parse_gallery(5, r#"var galleryinfo = {"title":"Ane | 姉弟","japanese_title":"姉弟"};"#).unwrap().link().title, "Ane | 姉弟");
        assert_eq!(parse_gallery(1, r#"var galleryinfo = {"title":"A","japanese_title":""}"#).unwrap().link().title, "A");
        assert_eq!(parse_gallery(1, r#"var galleryinfo = {"title":"","japanese_title":null}"#), None);
        assert_eq!(parse_gallery(1, "<html>"), None);
    }

    #[test]
    fn a_title_is_compared_without_the_spaces_and_the_marks() {
        assert_eq!(squash("Kyoudai de  Shitara-Ikemasen!"), "kyoudaideshitaraikemasen");
        assert_eq!(squash("姉弟で シたらいけません。"), squash("姉弟でシたらいけません"));
        assert_eq!(squash(" -!?"), "");
    }

    struct Broken;

    impl Fetch for Broken {
        async fn get(&mut self, _: &str, _: Option<(u64, u64)>) -> Result<Vec<u8>, String> {
            Ok(b"<html>".to_vec())
        }
    }

    #[tokio::test]
    async fn a_broken_site_is_an_error() {
        assert!(gallery_ids(&mut Broken, "kyoudai", 10).await.is_err(), "the version is not a number");
    }
}
