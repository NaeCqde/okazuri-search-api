# Okazuri Search API

[English](README.en.md) | 日本語

[Okazuri BOT](https://github.com/NaeCqde/okazuri_bot) の検索サーバーです。実行ファイル 1 つで、必要なものはすべて中に
入っています。機能は 2 つあり、それぞれ `config.toml` でオン・オフを決めます。

- **検索**(`[search]`、既定はオフ): `GET /okazuri/search?q=` が、BOT の知っているすべてのサイト(DLsite、FANZA、momon:GA など)の
  検索を使い、一覧ページに出ている作品の情報(タイトルと作品ページの path)を保存して、ローマ字にしたクエリの語をすべて含むタイトル(原文とローマ字の両方を保存します)の
  作品を返します。雑誌の号は、Worker の D1 データベースの写しから、事前に検索しておけます(写しはサーバーが最新に保ちます)。
  [docs/SEARCH.md](docs/SEARCH.md)
- **読みの変換**(常にオン。切り替えはありません): `GET /romaji?q=` が日本語をローマ字にします。読みの変換 API だけだった
  ころと同じです。

```
GET /okazuri/search?q=快楽天 2026 7
-> COMIC快楽天 2026年07月号   (DLsite, FANZA, ...)   COMIC快楽天ビースト 2026年07月号   ...

GET /romaji?q=チートアイテム管理局のお仕事
-> cheat item kanri kyoku no oshigoto
```

## 動かす

```
okazuri-search-api                       127.0.0.1:8000 でサーバーを起動
okazuri-search-api serve --port 9000     オプション: --config <パス> --host <アドレス> --port <ポート>
okazuri-search-api search <語>...        1 回検索(サーバーなし)。--live off|diff|on --limit <n>
okazuri-search-api sync                  D1 データベースの雑誌のテーブルを今すぐ写す
okazuri-search-api prefetch              その写しの号を 1 周、事前検索する。--limit <n>
okazuri-search-api convert "文"...       ローマ字を表示(サーバーなし)。文がなければ標準入力の各行
okazuri-search-api --help
```

```
curl "http://127.0.0.1:8000/okazuri/search?q=%E5%BF%AB%E6%A5%BD%E5%A4%A9"
curl "http://127.0.0.1:8000/romaji?q=%E3%83%81%E3%83%BC%E3%83%88"        # cheat
```

デモサーバーを <https://okazuri-api.nae.quest/> で公開しています(例: <https://okazuri-api.nae.quest/okazuri/search?q=a>)。

クエリは UTF-8 をパーセントエンコードしたものです。エンドポイントは [docs/API.md](docs/API.md) にあります。

## 設定

カレントディレクトリの `config.toml`(雛形は `config.example.toml`。どのキーも省略できます)。

| セクション | 内容 | 既定 |
|---|---|---|
| `[server]` | `host` / `port`。既定の `127.0.0.1` はこのマシンだけです。外に出すときはリバースプロキシを前に置きます。 | |
| (なし) | ローマ字への変換: `/romaji` と、検索のクエリ・タイトルのローマ字化。切り替えはありません。 | **常にオン** |
| `[search]` | `/okazuri/search` と、1 つ目の SQLite ファイル(`db_path`)。 | オフ |
| `[d1]` | Worker の D1 データベースの雑誌のテーブルの写し。2 つ目の SQLite ファイル(`mirror_path`)に入れます。 | オフ |
| `[prefetch]` | その写しのすべての号を、事前に検索します。 | オフ |
| `[cron]` | サーバーが決めた時刻に送る HTTP リクエスト(`[[cron.jobs]]`)。値の中の `${NAME}` は環境変数です。 | オフ |
| `[convert]`、`[exceptions]` | `/romaji` のクエリの最大長。辞書から読まずに、決めた綴りで書く語。 | |

データベースのファイルは 2 つです。`search.db`(サイトが一覧に出した作品)と `d1-mirror.db`(D1 の写し)。どちらも [SeaORM](https://www.sea-ql.org/SeaORM/) で読み書きします(テーブルは `src/entity/` のエンティティです)。
どちらも、なければ作られます。

## ダウンロード

push のたびに、実行ファイルがそのまま(アーカイブにせず)リリースの最上位に置かれます。同じ場所に Changes.md、LICENSE、設定の雛形 `config.example.toml` も置かれます。

| 実行ファイル                                            |                                                                                              |
| ------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `okazuri-search-api-Linux-x86_64-musl`                  | **静的リンク**で glibc に依存しません。どの Linux x86_64 サーバー(VPS、コンテナ)でも動きます |
| `okazuri-search-api-Linux-x86_64`、`-Linux-aarch64`     | glibc 版                                                                                     |
| `okazuri-search-api-Windows-x86_64`、`-Windows-aarch64` |                                                                                              |
| `okazuri-search-api-macOS-aarch64`                      |                                                                                              |

実行ファイルは約 190 MB です(UniDic と SQLite を埋め込んでいるため)。サーバーでの動かし方は
[docs/DEPLOY.md](docs/DEPLOY.md) にあります。

## ビルド

```
cargo build --release          # 初回は UniDic をダウンロードするのでネットワークが要ります
cargo test
```

リリースビルドは、実行ファイルを `Cargo.toml` の隣にコピーします(`OKAZURI_SEARCH_API_NO_COPY=1` で止められます)。
musl 向けは、`rustup target add x86_64-unknown-linux-musl` と `musl-tools`(`apt install musl-tools`)を入れて、
`cargo build --release --target x86_64-unknown-linux-musl` です。TLS(rustls)と SQLite は C のコードで、`musl-gcc` が
コンパイルします。

## 仕組み

- [docs/SEARCH.md](docs/SEARCH.md): 検索、サイト、2 つのデータベース、D1 の写し、事前検索。
- [docs/ALGORITHM.md](docs/ALGORITHM.md): ローマ字: 辞書、cutlet から移した規則、足した処理、Python 版との近さ。

## ライセンス

MIT
