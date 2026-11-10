# Changes

What the latest change (`feat: the search engine`) contains. The file is rewritten from scratch for each commit.

## What was done

The pronounce API became the search server of `okazuri_bot`, and was renamed `okazuri-search-api` (the crate, the
executable, `OKAZURI_SEARCH_API_NO_COPY`, the archives of the CI). Its conversion is now called **romaji** everywhere:
the endpoint is `GET /romaji` (it was `/pronounce`), and the search turns every query into romaji. The conversion has
no switch and is always on; a `[pronounce]` or `[romaji]` section in `config.toml` is ignored. The other functions are
switched in `config.toml` (`config.example.toml`).

DLsite and FANZA are asked a `×` as it was typed (`COMIC E×E 11`); the other sites get `x`. The bot writes the sign as
`x` already, so an `x` between two capitals (`ExE`) is put back as `×` for the two. The searches of the two are saved
under the query with the `×`.

DLsite and FANZA are asked a query without Japanese **as a phrase** (in double quotes; `{p}`/`{p+}` in `sites.rs`).
Their search finds the words anywhere and lists the newest first, so an old issue (`COMIC E×E 11`) was on page 3 of
DLsite and nowhere in FANZA. A query with Japanese is asked as it was.

- **Search** (`[search]`, off by default; `src/engine.rs`, `src/sites.rs`, `src/query.rs`, `src/store.rs`,
  [docs/SEARCH.md](docs/SEARCH.md)): `GET /okazuri/search?q=` asks the search of every site of
  `okazuri_bot/src/websites.ts`, reads the links of works from the list page, saves **the title of the item as the
  site writes it and the path of its page** (no image, no address of an image), and answers with the saved works whose
  title has **all the words** of the query (AND, any order; case, width, white space and `×`/`x` do not matter; `07` is
  the number 7). Nothing is added to the query: `hentai` was for web search engines. `GET /okazuri/sites` lists the
  sites. `search` is also a command (`okazuri-search-api search <words>`).
  - 15 of the 28 sites can be searched with a plain request (checked live). The others answer a Cloudflare challenge or
    need JavaScript, or are closed; they are listed with the reason and left out. RokuHentai is read from the JSON its
    JavaScript uses. Asking again does not pass a Cloudflare challenge (4 tries for each of the 7 sites, all challenged).
    Hitomi.La (index files, dozens of requests per query) and Anime-Sharing (a POST with a token, 503 for a guest) are not
    searched; the analysis is in [docs/SEARCH.md](docs/SEARCH.md).
  - A site that finds works by their Japanese title (all but EHentai and HentaiEra, checked with 快楽天) is asked **what was
    written and its romaji** when the query has Japanese in it; the other sites, and a query without Japanese, are asked the
    romaji only. `sites[].queries` says which; `GET /okazuri/sites` has `japanese`.
  - The title is read from the part of the card that holds it for the sites whose link text has a count, a price or
    a date with it (FANZA, momon:GA, Nya:Hentai, JComic, R18Best).
  - **`live=off|diff|on`**: `off` asks no site (the database answers, even if it is old), `diff` (the default) asks only the combinations
    of query and site whose answer is missing or older than `ttl_hours` (**a month**, 720: the same validity for a request and for the
    prefetch; the others are `cached` in `sites`), `on` asks every site, cached or not (`search --live on`) and saves the works that are **new** (a work in the database
    already is left as it is; `isNew` says which are new), then answers from the database, which also has what was saved
    before. `live=0` answers from the database alone. A failure is not asked again within `retry_failed_minutes`; a site is not asked twice within `min_delay_ms`.
- **The name of the query has to be outside the parentheses of the title, the numbers may be anywhere** (`作品 (COMIC快楽天 2018年8月号)` is a
  work of the issue; `... 11-po (COMIC ExE 61)` is not `COMIC ExE 11`; `COMIC快楽天 (2018年8月号)` is the issue): `Terms::matches_title`.
- **A month of validity, and works that are gone**: `ttl_hours` is 720. When a query is asked again, the new answer replaces the old in
  one transaction (nothing is deleted first), and a work that the query listed before and no longer lists is removed, unless another
  query lists it or the list was cut at `max_results_per_site` (`listings` table, [docs/SEARCH.md](docs/SEARCH.md)).
- **Two SQLite files**: `search.db` (`works`, `searches`, `listings`) and `d1-mirror.db` (the copy of D1), both through **SeaORM**
  (`src/entity/`, `src/store.rs`, `src/mirror.rs`; rusqlite is gone). The database code is async now; the tables of
  `search.db` are made from the entities. 177Picyy is asked at most once in 20 seconds and R18Best in 5, and a 429 is asked once
  more after the `Retry-After` of the site. The copy of D1 is replaced only when nobody reads it (Windows cannot replace an open file; the prefetch reads it while the sync replaces it), and the new file is moved over the old one with no moment without a mirror. The bookmark of D1 moves with a write to any table, so
  an export is compared by a hash with the copy: the same data is not loaded again and the prefetch is not woken. A change of the data
  stops the running prefetch round and starts a new one; the prefetch logs each query (`n/total`). A slow site (177Picyy) is left
  out of the prefetch and asked one form of a query.
- **A copy of the D1 database** (`[d1]`, off by default; `src/d1.rs`): the Time Travel bookmark tells whether the data
  changed; if it did, the `magazines` and `numbers` tables are exported through the Cloudflare API (the whole database is
  refused by D1 because of its FTS5 table), downloaded from the signed URL, and loaded into the mirror, which replaces
  the old file in one step. `okazuri-search-api sync` does it once.
- **Searching ahead** (`[prefetch]`, off by default; `src/prefetch.rs`): the name of every issue of the copy becomes a query
  the way the bot makes it (`COMIC快楽天 2026年07月号` is `COMIC快楽天 2026 07`), also in romaji if asked, and is searched,
  newest first, skipping what is searched already. A change of the copy starts a round.
- **The romaji conversion has no switch** any more: the dictionary is always loaded, `/romaji` is always there, and every query and every title of the search is turned into romaji.
- Earlier, still here: the `[cron]` jobs (HTTP requests on a schedule).

## Tested

120 tests, among them the engine against a local server that plays three sites (works, an error, a Cloudflare
challenge), the D1 calls against a mock of the API (token, polling, the signed URL, the bookmark), and the
pages of the sites as they are written (the cards of FANZA, momon:GA, Nya:Hentai, JComic, R18Best).

Run for real: 15 sites searched (13 of them twice for a query in Japanese) (the second request from the database in 20 ms); a prefetch round over a
copy of the 908 issues of the catalogue.

Not run for real: the calls to the Cloudflare API with a token (`okazuri-search-api sync` does it once), and the builds of
the CI for musl and the arm targets (the SQLite library is C code).
