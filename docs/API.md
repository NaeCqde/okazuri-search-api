# HTTP API

A function that is off in `config.toml` has no route: its paths answer `404`. A bad request is a `422` with
`{"detail": "..."}`.

## `GET /okazuri/search` (`[search]`)

The works that the sites list for a query. With `live` (the default) every site is asked now, the works that are new
are saved, and the answer is every saved work, the new ones and what was saved before, whose title has all the words of
the query. The whole story is in [SEARCH.md](SEARCH.md).

| Query | |
|---|---|
| `q` | The words. Required, 1 to `[search] max_query_chars` characters (300 by default). White space separates them. It is turned into romaji first, always. |
| `live` | `off`: ask no site, answer from the database alone, even when it is out of date. `diff` (default): ask only the combinations of query and site that are missing or older than `[search] ttl_hours` (a month); the others are `cached`. `on`: ask every site, cached or not (`searched`; an earlier failure is asked again). In every case the works that are new are saved, and the answer is from the database, which has them and what was saved before. `0`/`false`/`no` are `off`, `1`/`true`/`yes` are `on`. |
| `limit` | The most works in the answer, 1 to `[search] max_results` (100 by default). |

```
GET /okazuri/search?q=快楽天 2026 7
{
  "input": "快楽天 2026 7",
  "query": "kairakuten 2026 7",
  "terms": ["kairakuten", "2026", "7"],
  "total": 12,
  "results": [
    {
      "website": "DLsite",
      "isLegal": true,
      "path": "/books/work/=/product_id/BJ03044883.html",
      "url": "https://www.dlsite.com/books/work/=/product_id/BJ03044883.html",
      "title": "COMIC快楽天 2026年11月号",
      "titleRomaji": "comic kairakuten 2026 nen 11 gatsu gou",
      "isNew": true
    }
  ],
  "sites": [
    { "website": "DLsite", "status": "searched", "count": 30, "error": null, "searchedAt": 1793181281 }
  ]
}
```

- `title` is the title of the work's item in the site's list, as the site writes it. `path` is the path of its page (and the
  parameters of the query string that name the work); `url` is the domain of the site put before it.
- `input` is `q` as it was sent (tidied up), and `query` is its **romaji**: what the sites were asked, and what the
  titles were matched with. A `q` without Japanese in it is its own romaji. `titleRomaji` is the romaji of `title`.
- `total` is how many saved works have all the words; `results` is cut to `limit`.
- `isNew` is true for a work that this request found on a site and saved: it was not in the database before. A work that is
  found again is not new, and its row is left as it is (it is written again only if its title changed).
- `sites` says what came of each site: `searched` (asked now), `cached` (`live=diff`, and asked less than `ttl_hours` ago: the database answers), `failed` (with the `error`) or `not searched` (`live=off`), with the `queries`
  the site is asked: `["快楽天", "kairakuten"]` for a site that finds Japanese titles when `q` has Japanese in it, `["kairakuten"]`
  otherwise (a `q` without Japanese, or a site that does not find Japanese titles: EHentai, HentaiEra). `count` is the works the queries listed together.
  The request is `200` even if some sites failed.
- The works are in the order of the sites, and in a site the ones saved last come first.
- A `422` for a missing or too long `q`, and for a `live` that is not `off`, `diff` or `on`.

## `GET /okazuri/sites` (`[search]`)

The sites of `okazuri_bot/src/websites.ts`, and whether each is searched.

```
[ { "name": "DLsite", "isLegal": true, "support": "ready", "hasSearch": true, "japanese": true, "active": true, "note": "" },
  { "name": "IMHentai", "isLegal": false, "support": "cloudflare", "hasSearch": true, "japanese": true, "active": false, "note": "" }, ... ]
```

`support` is `ready`, `cloudflare` (a challenge that needs a browser), `javascript` or `closed`. `japanese` is whether the site finds
works by their Japanese title: it is then asked a query that has Japanese in it as written as well as in romaji. `active` is whether this
server searches the site with the current `[search]` settings (`disabled_sites`, `try_unsupported`).

## `GET /romaji`

The romaji of a Japanese text, as `text/plain; charset=utf-8`.

| Query | |
|---|---|
| `q` | The text. Required, 1 to `[convert] max_query_chars` characters (300 by default). |
| `loanwords` | `romaji` (default) or `english`: how to write a loanword that is spelled in hiragana. |

```
GET /romaji?q=チートアイテム管理局のお仕事
cheat item kanri kyoku no oshigoto

GET /romaji?q=ナカよしふらすとれーしょん！
naka yoshi furasutore-shon!

GET /romaji?q=ナカよしふらすとれーしょん！&loanwords=english
naka yoshi frustration!
```

A bad request (`q` missing, empty or too long, an unknown `loanwords`) is a `422`. A text that cannot be converted is a
`500`.

The query string has to be percent-encoded as UTF-8 (`encodeURIComponent` in JavaScript). Sending it
through a shell on Windows often mangles Japanese: use a program that encodes it.

## `GET /health`

`ok`, once the server is listening. That is after the dictionary is loaded, so a connection that is
refused during start-up means it is not ready yet.

## Who calls it

`okazuri_bot` called `/romaji` (`src/romaji.ts`) to rewrite a search query into romaji. `/okazuri/search` is the
search that replaces the Bing search of the bot: the bot sends the title (or the name of the issue) and gets the pages
of the sites that have it.

## Replacing the Python server

`/romaji` answers like the FastAPI server in `okazuri_bot/api` (cutlet + pykakasi), with these differences:

- no `engine` parameter: the pykakasi engine is gone (it was only a fallback);
- the dictionary is UniDic 2.1.2 (lindera) instead of UniDic 2.3.0 (unidic-lite), so a few words are
  split or read differently. `docs/ALGORITHM.md` has how close the two are.
