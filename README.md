# Okazuri Search API

English | [日本語](README.ja.md)

The search server of [Okazuri BOT](https://github.com/NaeCqde/okazuri_bot). One executable, with everything it needs
inside it. It has two functions, and each is switched on or off in `config.toml`:

- **Search** (`[search]`, off by default): `GET /okazuri/search?q=` asks the search of every site that the bot knows
  (DLsite, FANZA, momon:GA, ...), saves what their list pages show of each work (the title and the path of its page),
  and answers with the saved works whose title has all the words of the query. The query is always turned into
  romaji first, and the titles are kept as the sites write them and in romaji, so that they are matched in romaji. Magazine issues can be searched ahead
  of time, from a copy of the Worker's D1 database that the server keeps up to date. [docs/SEARCH.md](docs/SEARCH.md)
- **Romaji** (always on, it has no switch): `GET /romaji?q=` turns Japanese text into romaji, the way it was when
  this was the romaji API alone.

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
okazuri-search-api search <words>...     search once (no server); --live off|diff|on --limit <n>
okazuri-search-api sync                  copy the magazine tables of the D1 database now
okazuri-search-api prefetch              search for the issues of that copy once through; --limit <n>
okazuri-search-api convert "text"...     print the romaji (no server); without text, every line of stdin
okazuri-search-api --help
```

```
curl "http://127.0.0.1:8000/okazuri/search?q=%E5%BF%AB%E6%A5%BD%E5%A4%A9"
curl "http://127.0.0.1:8000/romaji?q=%E3%83%81%E3%83%BC%E3%83%88"        # cheat
```

The query is percent-encoded UTF-8. The endpoints are in [docs/API.md](docs/API.md).

## Settings

`config.toml` in the current directory (the template is `config.example.toml`; every key is optional):

| Section | What | Default |
|---|---|---|
| `[server]` | `host` / `port`. `127.0.0.1` is this machine only: put a reverse proxy in front of it. | |
| (none) | The romaji conversion: `/romaji`, and the romaji of every search query and title. It has no switch. | **always on** |
| `[search]` | `/okazuri/search`, and the first SQLite file (`db_path`). | off |
| `[d1]` | A copy of the magazine tables of the Worker's D1 database, in the second SQLite file (`mirror_path`). | off |
| `[prefetch]` | Searching for every issue of that copy ahead of time. | off |
| `[cron]` | HTTP requests that the server sends on a schedule (`[[cron.jobs]]`). `${NAME}` in a value is an environment variable. | off |
| `[convert]`, `[exceptions]` | The longest `/romaji` query; words to write out instead of reading them from the dictionary. | |

The two database files are `search.db` (the works the sites listed) and `d1-mirror.db` (the copy of
D1). Both are made if they are not there. They are read and written with [SeaORM](https://www.sea-ql.org/SeaORM/): the tables are
the entities in `src/entity/`.

## Downloads

Every push builds the archives (the executable, the READMEs, the config template, the license):

| Archive                                                 |                                                                            |
| ------------------------------------------------------- | -------------------------------------------------------------------------- |
| `okazuri-search-api-Linux-x86_64-musl`                  | **static**, no glibc: runs on any Linux x86_64 server (a VPS, a container) |
| `okazuri-search-api-Linux-x86_64`, `-Linux-aarch64`     | glibc                                                                      |
| `okazuri-search-api-Windows-x86_64`, `-Windows-aarch64` |                                                                            |
| `okazuri-search-api-macOS-aarch64`                      |                                                                            |

The executable is about 190 MB: UniDic is linked into it (and SQLite, with sqlx). How to run it on a server is in
[docs/DEPLOY.md](docs/DEPLOY.md).

## Build

```
cargo build --release          # downloads UniDic on the first build, so it needs the network
cargo test
```

A release build copies the executable next to `Cargo.toml` (set `OKAZURI_SEARCH_API_NO_COPY=1` to turn that off).
For musl: `rustup target add x86_64-unknown-linux-musl`, install `musl-tools`
(`apt install musl-tools`), then `cargo build --release --target x86_64-unknown-linux-musl`. The TLS (rustls) and the
SQLite library are C code that `musl-gcc` compiles.

## How it works

- [docs/SEARCH.md](docs/SEARCH.md): the search, the sites, the two databases, the copy of D1, the prefetch.
- [docs/ALGORITHM.md](docs/ALGORITHM.md): the romaji: the dictionary, the rules that were taken from cutlet, what was
  added, and how close it is to the Python version.

## License

MIT
