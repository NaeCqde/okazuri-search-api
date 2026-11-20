# Okazuri Search API

English | [日本語](README.md)

The search server for [Okazuri BOT](https://github.com/NaeCqde/okazuri_bot). It is a single executable with everything
it needs built in. It has two features, and each can be switched on or off in `config.toml`:

- **Search** (`[search]`, off by default): `GET /okazuri/search?q=` searches every site the bot supports (DLsite, FANZA,
  momon:GA, ...), saves the title and page path of each work shown on their listing pages, and returns the saved works
  whose titles contain every word of the query. The query is always converted to romaji first, and titles are stored both
  as the sites write them and in romaji, so matching is done in romaji. Magazine issues can be searched in advance, based
  on a copy of the Worker's D1 database that the server keeps up to date. See [docs/SEARCH.md](docs/SEARCH.md).
- **Romaji** (always on, no switch): `GET /romaji?q=` converts Japanese text to romaji, just as it did when this was
  only a romaji API.

```
GET /okazuri/search?q=快楽天 2026 7
-> COMIC快楽天 2026年07月号   (DLsite, FANZA, ...)   COMIC快楽天ビースト 2026年07月号   ...

GET /romaji?q=チートアイテム管理局のお仕事
-> cheat item kanri kyoku no oshigoto
```

## Run it

```
okazuri-search-api                       start the server on 127.0.0.1:8000
okazuri-search-api serve --port 9000     options: --config <path> --host <address> --port <port>
okazuri-search-api search <words>...     run one search (no server needed); --live off|diff|force --limit <n>
okazuri-search-api sync                  copy the magazine tables of the D1 database right now
okazuri-search-api prefetch              search for every issue in that copy, once through; --limit <n>
okazuri-search-api reromanize            regenerate the romaji of the saved works and discard the search history of the ones that changed (stop the server first)
okazuri-search-api convert "text"...     print the romaji (no server needed); without text, convert each line of stdin
okazuri-search-api --help
```

```
curl "http://127.0.0.1:8000/okazuri/search?q=%E5%BF%AB%E6%A5%BD%E5%A4%A9"
curl "http://127.0.0.1:8000/romaji?q=%E3%83%81%E3%83%BC%E3%83%88"        # cheat
```

A demo server is available at <https://okazuri-api.nae.quest/> (for example <https://okazuri-api.nae.quest/okazuri/search?q=a>).

The query is percent-encoded UTF-8. The endpoints are in [docs/API.md](docs/API.md).

## Settings

`config.toml` in the current directory (the template is `config.example.toml`; every key is optional):

| Section | What | Default |
|---|---|---|
| `[server]` | `host` / `port`. `127.0.0.1` is reachable from this machine only; put a reverse proxy in front of it to expose it. | |
| `[search]` | `/okazuri/search`, and the first SQLite file (`db_path`). | off |
| `[d1]` | A copy of the magazine tables of the Worker's D1 database, stored in the second SQLite file (`mirror_path`). | off |
| `[prefetch]` | Searches in advance for every issue in that copy. | off |
| `[cron]` | HTTP requests the server sends on a schedule (`[[cron.jobs]]`). `${NAME}` in a value is an environment variable. | off |
| `[romaji]`, `[romaji.exceptions]` | The maximum `/romaji` query length; words to spell as specified instead of looking them up in the dictionary. The conversion itself cannot be switched off. | **always on** |

There are two database files: `search.db` (the works listed by the sites) and `d1-mirror.db` (the copy of D1).
Both are created if they do not exist, and both are accessed through [SeaORM](https://www.sea-ql.org/SeaORM/); the tables are
defined by the entities in `src/entity/`.

## Downloads

Every push builds a release. The executables are uploaded as they are (not archived) to the top level of the release, together with Changes.md, the license and the config template `config.example.toml`:

| Executable                                               |                                                                            |
| ------------------------------------------------------- | -------------------------------------------------------------------------- |
| `okazuri-search-api-Linux-x86_64-musl`                  | **statically linked**, no glibc needed: runs on any Linux x86_64 server (VPS, container) |
| `okazuri-search-api-Linux-aarch64-musl`                 | the same, static, for Linux aarch64 (ARM servers, Raspberry Pi)           |
| `okazuri-search-api-Windows-x86_64`                     |                                                                            |
| `okazuri-search-api-macOS-aarch64`                      |                                                                            |

The executable is about 190 MB because UniDic and SQLite are built into it. For how to run it on a server, see
[docs/DEPLOY.md](docs/DEPLOY.md).

## Build

```
cargo build --release          # downloads UniDic on the first build, so it needs the network
cargo test
```

A release build copies the executable next to `Cargo.toml` (set `OKAZURI_SEARCH_API_NO_COPY=1` to turn that off).
For musl, run `rustup target add x86_64-unknown-linux-musl`, install `musl-tools`
(`apt install musl-tools`), then run `cargo build --release --target x86_64-unknown-linux-musl`. TLS (rustls) and
SQLite include C code, which `musl-gcc` compiles.

## How it works

- [docs/SEARCH.md](docs/SEARCH.md): the search, the supported sites, the two databases, the copy of D1, and prefetching.
- [docs/ALGORITHM.md](docs/ALGORITHM.md): how the romaji conversion works: the dictionary, the rules ported from
  cutlet, what was added, and how closely it matches the Python version.

## License

MIT
