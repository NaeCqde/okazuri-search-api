# Okazuri Search API

[English](README.en.md) | 日本語

[Okazuri BOT](https://github.com/NaeCqde/okazuri_bot) の検索サーバーです。実行ファイル 1 つに、必要なものがすべて
入っています。機能は 2 つあり、`config.toml` でそれぞれオン・オフを切り替えられます。

- **検索**(`[search]`、既定はオフ): `GET /okazuri/search?q=` は、BOT が対応しているすべてのサイト(DLsite、FANZA、
  momon:GA など)で検索し、一覧ページに載っている作品のタイトルと作品ページの path を保存します。そのうえで、ローマ字にした
  クエリの語をすべて含むタイトルの作品を返します(タイトルは原文とローマ字の両方を保存します)。雑誌の号は、Worker の D1
  データベースの写しをもとに、あらかじめ検索しておけます(写しはサーバーが最新の状態に保ちます)。
  詳しくは [docs/SEARCH.md](docs/SEARCH.md) を見てください。
- **ローマ字変換**(常にオン。切り替えはありません): `GET /romaji?q=` が日本語をローマ字にします。ローマ字変換 API だけ
  だったころと同じ動作です。

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
okazuri-search-api search <語>...        1 回だけ検索する(サーバーは使わない)。--live off|diff|force --limit <n>
okazuri-search-api sync                  D1 データベースの雑誌のテーブルを今すぐ写し取る
okazuri-search-api prefetch              写しにある号を 1 周分、事前に検索する。--limit <n>
okazuri-search-api reromanize            保存済み作品のローマ字を作り直し、変わった作品の検索履歴を消す(先にサーバーを止めてください)
okazuri-search-api convert "文"...       ローマ字を表示する(サーバーは使わない)。文を省くと標準入力の各行を変換する
okazuri-search-api --help
```

```
curl "http://127.0.0.1:8000/okazuri/search?q=%E5%BF%AB%E6%A5%BD%E5%A4%A9"
curl "http://127.0.0.1:8000/romaji?q=%E3%83%81%E3%83%BC%E3%83%88"        # cheat
```

デモサーバーを <https://okazuri-api.nae.quest/> で公開しています(例: <https://okazuri-api.nae.quest/okazuri/search?q=a>)。

クエリは UTF-8 をパーセントエンコードして渡します。エンドポイントの一覧は [docs/API.md](docs/API.md) にあります。

## 設定

カレントディレクトリの `config.toml`(雛形は `config.example.toml`。どのキーも省略できます)。

| セクション | 内容 | 既定 |
|---|---|---|
| `[server]` | `host` / `port`。既定の `127.0.0.1` はこのマシンからしか届きません。外部に公開するときは、前にリバースプロキシを置いてください。 | |
| (なし) | ローマ字変換: `/romaji` と、検索のクエリ・タイトルのローマ字化。切り替えはできません。 | **常にオン** |
| `[search]` | `/okazuri/search` と、1 つ目の SQLite ファイル(`db_path`)。 | オフ |
| `[d1]` | Worker の D1 データベースにある雑誌のテーブルの写し。2 つ目の SQLite ファイル(`mirror_path`)に保存します。 | オフ |
| `[prefetch]` | 写しにあるすべての号を、あらかじめ検索します。 | オフ |
| `[cron]` | 決めた時刻にサーバーが送る HTTP リクエスト(`[[cron.jobs]]`)。値の中の `${NAME}` は環境変数です。 | オフ |
| `[convert]`、`[exceptions]` | `/romaji` のクエリの最大長。辞書を引かず、指定した綴りで書く語。 | |

データベースのファイルは 2 つあります。`search.db`(サイトの一覧に載っていた作品)と `d1-mirror.db`(D1 の写し)です。
どちらも [SeaORM](https://www.sea-ql.org/SeaORM/) で読み書きし(テーブルは `src/entity/` のエンティティです)、ファイルがなければ自動で作られます。

## ダウンロード

push のたびに、実行ファイルがアーカイブにされず、そのままリリースの最上位に置かれます。同じ場所に Changes.md、LICENSE、設定の雛形 `config.example.toml` も置かれます。

| 実行ファイル                                            |                                                                                              |
| ------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `okazuri-search-api-Linux-x86_64-musl`                  | **静的リンク**で glibc に依存せず、どの Linux x86_64 サーバー(VPS、コンテナ)でも動きます |
| `okazuri-search-api-Linux-x86_64`、`-Linux-aarch64`     | glibc 版                                                                                     |
| `okazuri-search-api-Windows-x86_64`、`-Windows-aarch64` |                                                                                              |
| `okazuri-search-api-macOS-aarch64`                      |                                                                                              |

実行ファイルは約 190 MB あります(UniDic と SQLite を埋め込んでいるためです)。サーバーでの動かし方は
[docs/DEPLOY.md](docs/DEPLOY.md) を見てください。

## ビルド

```
cargo build --release          # 初回は UniDic をダウンロードするのでネットワークが要ります
cargo test
```

リリースビルドでは、実行ファイルが `Cargo.toml` の隣にコピーされます(`OKAZURI_SEARCH_API_NO_COPY=1` でやめられます)。
musl 向けにビルドするには、`rustup target add x86_64-unknown-linux-musl` を実行し、`musl-tools`(`apt install musl-tools`)を
入れてから、`cargo build --release --target x86_64-unknown-linux-musl` を実行します。TLS(rustls)と SQLite は C のコードを含むため、
`musl-gcc` でコンパイルされます。

## 仕組み

- [docs/SEARCH.md](docs/SEARCH.md): 検索、対応サイト、2 つのデータベース、D1 の写し、事前検索。
- [docs/ALGORITHM.md](docs/ALGORITHM.md): ローマ字変換の仕組み。辞書、cutlet から移植した規則、追加した処理、Python 版との差。

## ライセンス

MIT
