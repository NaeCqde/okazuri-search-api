# The search engine

`GET /okazuri/search?q=<words>` answers with the works that the sites list for a query. It is the part of
`okazuri-search-api` that is switched on with `[search] enabled = true`.

```
 request ──► ask every site ──► read the list page ──► save title + path ──► find ──► answer
              (every site, now)      (links of works)     (search.db, new ones)     (AND of the words)
```

## What a request does

1. The query is tidied up (white space folded, `×` written as `x`) and **turned into romaji, always** (`快楽天 2026 7`
   is `kairakuten 2026 7`; text without Japanese is its own romaji). The romaji is what is searched for. Nothing is added to
   it: the sites have a search of their own, and `hentai` is only for web search engines. The exception is `×`: DLsite and
   FANZA (`keep_times` in `sites.rs`) are asked `COMIC E×E 11` as it was typed, the other sites `COMIC ExE 11`. The answer
   (`query`) and the matching always use `x`.
2. Each site is asked either the romaji alone, or **both what was written and its romaji**: a site that finds works by their
   Japanese title (all but EHentai and HentaiEra, which answer `快楽天` with nothing) is asked the query as written too when it has
   Japanese in it, so `快楽天 2026` is asked as `快楽天 2026` and as `kairakuten 2026`. A query without Japanese is only romaji. The works
   of both answers go into the database and are matched the same way. (`GET /okazuri/sites` says which sites are `japanese`.)
3. With `live` (the default), every site that can be searched is asked now. The sites are asked at the same time, and a site is
   never asked twice within `min_delay_ms` (177Picyy, which answers 429 to a rush, within 20 seconds; R18Best, within 5). A 429 is
   answered once with another try after the `Retry-After` of the site (5 seconds if it gives none, at most 30). (`live=off` asks nobody and goes on with the database.)
4. The links of the answer that are works' pages are read. A link is a work when it matches the pattern that
   `okazuri_bot/src/websites.ts` uses for the site.
5. **What is saved is what the list page shows of the work**: its title, as the page writes it, **the romaji of that
   title** (made with the same conversion as the query, to match it), and the path of its page (no scheme, no domain). No
   image and no address of an image. **Only what is new is written**: a work that is in the database already is left as it
   is (its row is written again only if its title changed), and the works that were not there are added. The title is the one of the *item of the list*, not
   the title of the page; for the sites whose items hold more than the title (a count of likes, a price, a date, the
   author) it is read from the part of the item that has the title.
6. The answer is **every saved work whose romaji title has all the words of the (romaji) query** (AND, any order), whether it is new or was saved before, not only the
   works this request found. A work is one row whatever query found it.

### Fetching again, and works that are gone

A query is asked to a site again only after `[search] ttl_hours` (720: a month) — for a request as well as for the prefetch.
Nothing is deleted to make room: the new answer of a site is written in one transaction, so the old works are what a search
sees until the new ones are in. The database (`listings`) knows which query listed which work on which site. When a query
is asked again and a work that it listed the time before is **not on the list now**, the work is **removed** — if no other
query lists it, and if the list was whole. A list as long as `max_results_per_site` may have been cut short, and then a work that
is missing may just be beyond the cut, so it stays. A failed try changes nothing. A work from before `listings` existed
(no query is known to have listed it) is never removed.

## Matching

- Case, full width and half width (`ＣＯＭＩＣ`), white space (`COMIC 快楽天` and `COMIC快楽天`) and `×` against `x` do
  not matter.
- A word that is a number alone is a number of the title: `07` finds `2026年7月号`, `6` does not find `2026`.
- **The name has to be outside the parentheses** of the title, the numbers may be anywhere in it: what is in `( )` (or `（ ）`) is not
  matched against the words of the query. `彼女にできること (COMIC快楽天 2018年8月号)` is a work of the issue, not the issue, so it is not an
  answer to `COMIC快楽天 2018 8`; `[Mutsutake] Meguri Dokoro 11-po (COMIC ExE 61) [Chinese]` is not an answer to `COMIC ExE 11` (`COMIC ExE`
  is inside). `COMIC快楽天 2018年8月号 (デジタル特装版)` and `COMIC快楽天 (2018年8月号)` are answers. Brackets (`[ ]`, `【 】`) are not parentheses.
- So `快楽天 2026 7` finds `COMIC快楽天 2026年07月号` (the query `kairakuten 2026 7` against `comic kairakuten 2026-07`), and `exe 11` finds `COMIC ExE 11`. A query written in romaji finds the same works as the same words in Japanese.

## Request

| Query | |
|---|---|
| `q` | The words. Required, at most `[search] max_query_chars` characters. |
| `live=off` | Ask no site: answer from the database alone, even when what it has is out of date. |
| `live=diff` (default) | Ask only the combinations of query and site that are missing or older than `ttl_hours` (a month); for the others the database answers (`cached`). |
| `live=force` | Ask every site, cached or not (`searched`; a failure of an earlier try is asked again). `on` (the old name), `1`, `true` and `yes` mean the same. |
| `limit=n` | The most works in the answer (at most `[search] max_results`). |
| `exclude=A,B` | The sites (names as `GET /okazuri/sites` lists them, case does not matter, separated by commas) that are left out: they are not asked, their saved works are not in the answer, and they are not in `sites`. A name that is no site is a 422. The translation is not affected: `translator` says where it comes from. |
| `translator=auto` (default) | A query with Japanese in it is searched in English too: the English title comes from Hitomi.La (`/hitomila/translate`), and from Google Translate (`/google/translate`) when Hitomi.La fails or has nothing. The works of both searches are in `results`, and `translation` says the title (`{"text", "source"}`). |
| `translator=hitomi` | Hitomi.La only: when it fails or has nothing, the English search is left out (`translation` is `null`). |
| `translator=google` | Google Translate only. |
| `translator=none` | No English search. |

`live` also tells the translation what to do: the saved translation is used (`diff`), only the saved one (`off`), or the source is asked again (`force`).

The answer:

```json
{
  "input": "COMIC快楽天 2026 11",
  "query": "comic kairakuten 2026 11",
  "terms": ["comic快楽天", "2026", "11"],
  "total": 28,
  "results": [
    { "website": "FANZA", "isLegal": true, "path": "/product/25199/b915awnmg04607/",
      "url": "https://book.dmm.co.jp/product/25199/b915awnmg04607/", "title": "COMIC快楽天 2026年11月号",
      "titleRomaji": "comic kairakuten 2026 nen 11 gatsu gou" }
  ],
  "sites": [
    { "website": "FANZA", "status": "searched", "count": 23, "error": null, "searchedAt": 1793181281 }
  ]
}
```

`status` is `searched` (asked now), `failed` (the `error` says why) or `not searched` (`live=0`); `queries` are the queries the site is asked
(one or two, see above) and `count` the works they listed together. A site that fails does not stop the others. `total` is the number of works that have all the words; `results` may be shorter. A page is one work in the database and one result: a page that a site writes with and without the slash at the end (`/g/1/`, `/g/1`) is saved once. A page saved with the slash that the site lists without it later is **renamed** to the path without (the same row, so `first_seen` stays, and the lists that name it follow); one listed in both forms at once is saved without the slash (a site that writes it that way does not need it); one that is saved without the slash stays so; one that is only ever listed with the slash keeps it, and works with the same address (also with or without the `/` at the end, or the `#` part, in lower case) are one, the first stays, and it is `isNew` when any of them is.

## The same search on Hitomi.La: `GET /hitomila/search`

`GET /hitomila/search?q=…` is `/okazuri/search` with every other site left out (`exclude`): the same parameters (`live`, `limit`,
`translator`) and the same answer (`input`, `query`, `terms`, `total`, `results`, `sites` with Hitomi.La alone, `translation`). It has no route (404)
when Hitomi.La is left out of `[search]`.

## The sites

### `GET /hitomila/translate?q=<text>`: Japanese to English, English to Japanese

A translation of the title of a work, from the titles of Hitomi.La: a gallery has its title in English and in Japanese, and the
site keeps the words of both in one index. The language of `q` says the direction: Japanese (it has kana or kanji) to English, or
English (its letters are all Latin) to Japanese. Any other text is a 422: only these two are translated.

```json
{
  "input": "姉弟でシたらいけません",
  "title": "姉弟でシたらいけません",
  "from": "ja",
  "to": "en",
  "translation": "Kyoudai de Shitara Ikemasen",
  "results": [
    {
      "id": 3863504,
      "url": "https://hitomi.la/galleries/3863504.html",
      "english": "Kyoudai de Shitara Ikemasen",
      "japanese": "姉弟でシたらいけません",
      "exact": true
    }
  ],
  "saved": false
}
```

**Only the title of the work is translated**: what the site puts around it is left out before Hitomi.La is asked (and before the translation is saved), and `title` says what was left: the event, the circle and the artist at the front (`(C103) [Circle (Artist)]`), the series, the tags and the language at the back (`(Series) [English] [DL版]`), and the other title after ` | ` or ` / ` (the one in the language of the text is kept). `[Yamamoto Zenzen] 学園サポ日記 comix 第1話 (COMIC Kuriberon DUMA 2025-09 Vol.76) [MTL]` is asked as `学園サポ日記 comix 第1話`. A text with nothing left (`【推しの子】`) is asked as it is. Google Translate gets the whole text.

`translation` is the text of the first result in the other language (`null` when nothing answered). The Japanese gallery has the
Japanese title, the galleries in English have the English one (the site has the Japanese words in the index of the galleries of
the other languages too, so a text in Japanese finds them all). The galleries are **the same work** when their artists, groups,
parodies and kind are the same and the pages are about as many (3 at most apart: a translation can add a page of credits); the
tags (`big ass`, `paizuri`...) are not compared, they differ from one upload to another, and the romaji in the titles is not
needed. So:

- Japanese to English: the gallery whose Japanese title has the text, and the galleries in English that are the same work;
- English to Japanese: the galleries in English whose title has the text, and the Japanese gallery that is the same work (when the
  first search does not have it, the romaji after the bar of `English | romaji` is looked up).

A gallery with neither an artist nor a group is never the same work as another. `exact` says the text is the whole title (spaces,
marks and case aside); those come first.

The answer is **saved** in its own tables of the search database: `translation_lookups` (`key`: the text as it is compared, letters
and digits in lower case; `text`; `from_lang`; `looked_at`; `count`: a note of the last time the text was asked) and
`translations` (`key`, `gallery_id`, `position`, `url`, `english`, `japanese`, `exact`). `live` is the one of the search: `diff`
(default) answers with the saved translations, **however old** (they are kept for good), and asks the site for a text that has none saved: a text
that was never asked, and a text that nothing answered (it is asked every time, the site may have the work now); `force` asks the site always. There is **no `live=off`** here (a 422): the saved answer is the answer of `diff`. `saved` says that the answer is the saved one.
A site that does not answer is a 502; no `q`, a `q` that is not Japanese or English, or a `live` that is not diff or force is a
422. The route is there when `[search]` is on.

### `GET /google/translate?q=<text>`: the same from Google Translate

The same rules for `q` (Japanese or English, the direction follows), and the same tables: the translation is saved with the source
`google` (kept for good). `live=diff` (default) answers with the saved one, and `live=force` asks Google again. There is **no
`live=off`** for Google Translate (a 422): it is asked whenever nothing is saved (in the search, `off` is `diff` for the Google part; `/auto/translate` refuses it too). A translation that was saved for the other direction answers too, when its title is the whole text
(`十` → `Ten` saved, so `Ten` → `十`). A translation from Japanese to English does not start with `The` (`The Phaethon Assault` →
`Phaethon Assault`): it is taken off before it is saved; the translation of Hitomi.La is left as the site writes it.

Google Translate is asked the way its web page asks (the RPC `MkEWBc` of `translate.google.com`, the request of the model that the
page calls Advanced, "Gemini による正確性重視の翻訳"): there is no key and no promise, and Google can change it. Only the Advanced model is used, and the test
`gtranslate::tests::the_advanced_model_...` (`cargo test -- --ignored`, it asks Google) is how a change is noticed: it works when
`パエトーン強襲戦` comes back as `The Phaethon Assault` (the classic model says `The Battle of Phaeton`), and any other answer means that
Google changed. Without the bridge the test fails: the page's own request has the same URL parameters and body as this program's, plus the
header `x-goog-batchexecute-bgr` (a token of Google's bot check); with it the answer is `The Phaethon Assault` (`[[3],[[6,3]]]`), without it
`Assault on Phaeton` (`[[0],[[6,1]]]`). The token is made for one text (the same text again is accepted, also with another `_reqid`) and is
checked with the `User-Agent` and `Referer` of the page, not with its cookies. It cannot be made without running Google's script, so
`tools/google-translate-bridge` (a Chrome with no window, `node server.mjs`, see its README) makes it: `GET /token` opens the page with the
text, holds back the page's own request and hands it over, and this program sends it (`reqwest`, with gzip). `[search] google_bridge_url`
(and `google_bridge_token`) is its address; when it is set and it is down, Google Translate fails (there is no fallback to the classic answer,
which would be saved for good). To run the test with the bridge: `OKAZURI_GOOGLE_BRIDGE=http://127.0.0.1:8130 cargo test -- --ignored`.

```json
{ "input": "パエトーン強襲戦", "from": "ja", "to": "en", "translation": "Assault on Phaeton", "saved": false }
```

The saved rows of Hitomi.La and of Google are separate (`translations.source`).

### `GET /auto/translate?q=<text>`

Hitomi.La first (`/hitomila/translate`), and Google Translate (`/google/translate`) when Hitomi.La fails or has nothing. The same
rules for `q` and `live`; the answer is the first translation, and `source` says who gave it:

```json
{ "input": "パエトーン強襲戦", "from": "ja", "to": "en", "translation": "ZenZero Gyaku Ra Goudou \"Phaethon Assault\"", "source": "hitomi", "saved": false }
```

Both are saved as above; a 502 when neither has a translation. `live=off` is a 422 here too (Google is asked when nothing is saved). This is what `translator=auto` of the search uses.

`GET /okazuri/sites` lists them. Checked in 2026-10 from a home connection:

| | Sites |
|---|---|
| Searched | DLsite, FANZA, momon:GA, Nya:Hentai (now nyahentai.one), 同人の森, JComic, R18Best, Sukebei, RokuHentai, HentaiEnvy, HentaiEra, 177Picyy, Wnacg, AsmHentai, EHentai, Hitomi.La (16; all in Japanese and romaji, but HentaiEra and EHentai in romaji only) |
| Cloudflare challenge | IMHentai, HentaiOMG, HentaiCore, NijiGazo, NukiBooks, HentaiName, HentaiPaw: a browser is needed |
| JavaScript | Anime-Sharing (analysed, see below) |
| Closed | MasDr, JMwu, BatoTo, HentaiDesi |

The ones that cannot be searched are left out (`try_unsupported = true` tries them, and they fail). A site is one entry
of `SITES` in `src/sites.rs`: the pattern of `websites.ts`, a search URL with `{q}` in it (`{q+}` writes a space as `+`:
DLsite answers 403 to `%20` in a path), and, if the title is not the text of the link, where it is (`TITLES`).

### The sites that need JavaScript (analysed 2026-10)

- **RokuHentai**: the page is drawn by JavaScript, but from a JSON answer of the same address (`/_search?q=`: `manga-cards` holds
  the cards as HTML, `next` the next page). That answer is read, so the site is searched. The reader (`/id/0`) and the details
  (`/id`) are one work.
- **Hitomi.La** (`src/hitomi.rs`): the search is JavaScript over binary index files on `ltn.gold-usergeneratedcontent.net`, and the program reads
  the same files, with no browser. The SHA-256 of a word (first 4 bytes) is looked up in a B-tree (`galleriesindex/galleries.<version>.index`,
  nodes of 464 bytes read with Range requests; the version is in `galleriesindex/version`), which gives where the list of the
  galleries that have the word in their title is (in the `.data` file); the ids of the words are intersected (the shortest list first;
  a word that is in no title is left out, a word in more than 500,000 titles is too), the newest `[search] max_results_per_site`
  are kept, and the title of each is read from `galleries/<id>.js`. One query is some 20 requests of the index plus one per gallery,
  each 150 ms after the last (this site has its own delay, not `min_delay_ms`); the index is a static host that answers quickly.
  The words of the Japanese titles and of the English titles are in the same index, so a query in Japanese finds the English titles
  too, and the other way round. The work is saved as `/galleries/<id>.html` (the site redirects it to the page) with the title
  `<title> | <Japanese title>`: the romaji of that title has the Japanese words and the English ones, so a query in Japanese, in
  romaji or in English matches it.
- **Anime-Sharing**: the search is a POST with a token (XenForo), and the site answers a guest with its own 503 page.
- **A Cloudflare challenge** (7 sites) is not passed by asking again: it was tried 4 times for each of the 7 sites, and every
  answer was a challenge. A browser is needed.

## The two database files

Both are read and written with SeaORM. The tables are the entities in `src/entity/` (`work`, `search` for `search.db`; `magazine`,
`number`, `mirror_meta` for the mirror), `src/store.rs` is the search database and `src/mirror.rs` the copy of D1. The tables of
the search database are made when the file is opened (`CREATE TABLE IF NOT EXISTS`, from the entities), so there is nothing to
migrate. The entities of `magazine` and `number` list only the columns that are read, because D1 owns that schema.

| File | Setting | Content |
|---|---|---|
| `search.db` | `[search] db_path` | `works` (site, path, **title** as the site writes it, **title_romaji** its romaji, title_key the romaji folded to narrow the search, when it was first saved and last changed), `works_fts` (an FTS5 index with the trigram tokenizer over title_key: it finds the works whose key has a piece of 3 characters or more without reading every row; the table `works` is its content, and triggers keep it up to date; it is made again at the start when it has not as many works as the table) and `searches` (when each query was asked to each site, and whether it worked) |
| `d1-mirror.db` | `[d1] mirror_path` | the `magazines` and `numbers` tables of the Worker's D1 database, and the bookmark of the data they were copied from |

## The copy of D1 (`[d1]`)

D1 can export a database as an SQL file through the Cloudflare API: `POST .../d1/database/{id}/export` with
`output_format: "polling"` is polled until it is `complete`, and the SQL is downloaded from the signed URL it gives.
The Worker's database has a virtual table (`works_fts`, FTS5), which D1 refuses to export, so the tables to export are
named (`dump_options.tables`): `wrangler d1 export okazuri --remote --table=numbers` does the same.

Whether to export is told by the Time Travel bookmark (`GET .../time_travel/bookmark`), which costs no row reads and
moves on with every write **to any table** (the Worker writes its works all day). The bookmark of the data in the copy is kept in the copy.
When the two differ, the two tables are exported, and a hash of the SQL is compared with the one of the copy: the same
hash means that `magazines` and `numbers` did not change (only another table did), so the copy is left as it is, with the new
bookmark noted, and the prefetch is not told. Only a different hash builds the copy in a temporary file and replaces the old one in one step, so a broken export leaves the old copy alone.

An export blocks the database while it runs (a second or so for these two tables).

## Searching ahead (`[prefetch]`)

The name of every issue of the copy is turned into a query the way `okazuri_bot` does it (`COMIC快楽天 2026年07月号` is
`COMIC快楽天 2026 07`), newest first, and searched like a request: the sites are asked and the works saved. The engine
turns each into romaji like any query. Queries that are searched already and young (`[search] ttl_hours`, a month by default, the same as for a request; a failure: `retry_failed_minutes`) are skipped, so a round only does what
is new. A change of the data of the copy (not a mere move of the bookmark) **stops the round that is running and starts a new one** at
once. What was searched is remembered (`searches`: a query is skipped while it is younger than `ttl_hours` for every site, and
only the sites that are missing or old are asked), so a restart of the program, or of a round, goes on where it stopped. The log
says `prefetch: n/total "query"` for each query. A slow site (177Picyy: one request in 20 seconds) is left out of the prefetch,
because every query would wait for it; a request to `/okazuri/search` asks it, with one form of the query.

A query with Japanese in it is searched three ways: as written, in romaji (both by the search itself), and **in English**: when Hitomi.La is one of the sites, the prefetch asks Hitomi.La (as `/hitomila/translate` does; saved for good; never Google Translate), and searches the sites for the English titles it gives (3 at most), saving the works. This is also done for a query that was searched before, so the works of the English titles come after the update too. The log says `prefetch: "query" in English "title"`. A failure of the translation is a warning; the query itself is done.

### After the rules of the romaji changed (`reromanize`)

A work is saved with the romaji it had when it was listed, so after an update of the rules the saved romaji is the old one
until the work is listed again (a month). `okazuri-search-api reromanize` (the server stopped) makes the romaji of every saved
work again; a work whose romaji came out different is rewritten (title, dates and the order of the results stay), and the
searches that listed it are forgotten. It asks no site: the prefetch of the server, when it starts, finds those queries not
searched and asks the sites again, newest issue first. The queries that are not issues are asked again when somebody searches
for them.

## Crawling (`[crawl]`)

The searches only know the works that a query listed. With `[crawl] enabled = true` the server also walks the list of each site that has one
(`crawl.rs`: the index of Hitomi.La and the file of each gallery, the WordPress API of momon:GA and Nya:Hentai, the list pages of DLsite, Wnacg,
AsmHentai, HentaiEra and HentaiEnvy, the JSON of RokuHentai, the list of E-Hentai) and saves the title of every work, with no query. The
sites are walked at the same time, each one page after page, newest first.

- **Where it is:** table `crawls` (a row per site: the cursor of the walk, `done`, how many works it saved). A restart goes on from the cursor.
- **After the first walk:** at the `schedule` the newest pages are read again, up to a page with nothing new in it (`refresh_pages` at the most).
- **The prefetch** leaves out the sites whose first walk is done, and goes on asking the sites that have no list (Sukebei, JComic, ...).
- **The delay** of a site is kept by every request of the program together (`[search] min_delay_ms`, or `[search.site_delay_ms]` for one site).
- **More than one machine:** `shard = "k/N"` on each, then `export` on each (`okazuri-search-api export out.jsonl.br`; JSON Lines, brotli) and
  `okazuri-search-api import a.jsonl.br b.jsonl.br c.jsonl.br --mark-done Hitomi.La ...` on the server. A work that is saved already stays as it is.

## Not done

- A site behind a Cloudflare challenge and Anime-Sharing are not searched: this is plain HTTP requests, not a browser.
- The `[d1]` calls were written from the API as wrangler uses it and tested against a mock of it; run
  `okazuri-search-api sync` once with the token to see them work.

### How the works are narrowed (`SearchStore::candidates`)

The engine reads the works that have the longest word of the query (a number is not one) in their title key, and matches every word of the
query with them in the program. The word is looked up in `works_fts` (FTS5, trigram) when it has 3 characters or more and no more than
50 000 works have it; the works are then the ones of the index, the newest first. A word that is shorter, and a word that more than 50 000
works have, is read with `instr(title_key, word)` by walking the works from the newest, which stops after 50 000. The answer is the same
either way. (Measured on 5 million works: a rare word 12 to 80 ms instead of 2 to 3.4 s; a word that most works have 30 ms as before.)
