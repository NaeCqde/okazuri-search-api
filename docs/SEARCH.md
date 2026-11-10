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
   answered once with another try after the `Retry-After` of the site (5 seconds if it gives none, at most 30). (`live=0` asks nobody and goes on with the database.)
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
- So `快楽天 2026 7` finds `COMIC快楽天 2026年07月号` (the query `kairakuten 2026 7` against `comic kairakuten 2026 nen 7 gatsu
  gou`), and `exe 11` finds `COMIC ExE 11`. A query written in romaji finds the same works as the same words in Japanese.

## Request

| Query | |
|---|---|
| `q` | The words. Required, at most `[search] max_query_chars` characters. |
| `live=off` | Ask no site: answer from the database alone, even when what it has is out of date. |
| `live=diff` (default) | Ask only the combinations of query and site that are missing or older than `ttl_hours` (a month); for the others the database answers (`cached`). |
| `live=on` | Ask every site, cached or not (`searched`; a failure of an earlier try is asked again). |
| `limit=n` | The most works in the answer (at most `[search] max_results`). |

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
(one or two, see above) and `count` the works they listed together. A site that fails does not stop the others. `total` is the number of works that have all the words; `results` may be shorter.

## The sites

`GET /okazuri/sites` lists them. Checked in 2026-10 from a home connection:

| | Sites |
|---|---|
| Searched | DLsite, FANZA, momon:GA, Nya:Hentai (now nyahentai.one), 同人の森, JComic, R18Best, Sukebei, RokuHentai, HentaiEnvy, HentaiEra, 177Picyy, Wnacg, AsmHentai, EHentai (15; all in Japanese and romaji, but HentaiEra and EHentai in romaji only) |
| Cloudflare challenge | IMHentai, HentaiOMG, HentaiCore, NijiGazo, NukiBooks, HentaiName, HentaiPaw: a browser is needed |
| JavaScript | Hitomi.La, Anime-Sharing (analysed, see below) |
| Closed | MasDr, JMwu, BatoTo, HentaiDesi |

The ones that cannot be searched are left out (`try_unsupported = true` tries them, and they fail). A site is one entry
of `SITES` in `src/sites.rs`: the pattern of `websites.ts`, a search URL with `{q}` in it (`{q+}` writes a space as `+`:
DLsite answers 403 to `%20` in a path), and, if the title is not the text of the link, where it is (`TITLES`).

### The sites that need JavaScript (analysed 2026-10)

- **RokuHentai**: the page is drawn by JavaScript, but from a JSON answer of the same address (`/_search?q=`: `manga-cards` holds
  the cards as HTML, `next` the next page). That answer is read, so the site is searched. The reader (`/id/0`) and the details
  (`/id`) are one work.
- **Hitomi.La**: the search is JavaScript over binary index files on `ltn.gold-usergeneratedcontent.net`: the SHA-256 of a word
  (first 4 bytes) is looked up in a B-tree read with Range requests, which gives gallery ids, and the title of each id is one more
  file (`galleries/<id>.js`). It can be done without a browser, but it is dozens of requests for one query, so it is not.
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
| `search.db` | `[search] db_path` | `works` (site, path, **title** as the site writes it, **title_romaji** its romaji, title_key the romaji folded to narrow the search, when it was first saved and last changed) and `searches` (when each query was asked to each site, and whether it worked) |
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

## Not done

- A site behind a Cloudflare challenge, Hitomi.La and Anime-Sharing are not searched: this is plain HTTP requests, not a browser.
- The `[d1]` calls were written from the API as wrangler uses it and tested against a mock of it; run
  `okazuri-search-api sync` once with the token to see them work.
