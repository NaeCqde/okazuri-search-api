# okazuri-search-api 仕様

[Okazuri BOT](https://github.com/NaeCqde/okazuri_bot) のための検索サーバー。1 つの実行ファイルに、日本語→ローマ字変換、雑誌・作品の検索、Cloudflare D1 の写し、事前検索、定期リクエスト(cron)が入っている。
仕様を変えたときは、このファイルも必ず更新する。細部は `docs/` にある:

| 文書 | 内容 |
|---|---|
| `docs/API.md` | HTTP の API(パラメーター、応答の形、状態コード) |
| `docs/SEARCH.md` | 検索エンジン(リクエストがしたこと、照合、サイト、2 つのデータベース、D1 の写し、事前検索) |
| `docs/ALGORITHM.md` | ローマ字の作り方(cutlet を元にした規則と、足した規則) |
| `docs/DEPLOY.md` | サーバーでの動かし方(systemd、cron、リバースプロキシ、Docker) |

## 構成

| ファイル | 役割 |
|---|---|
| `main.rs`, `cli.rs` | 起動、サブコマンド(`serve`、`search`、`sync`、`prefetch`、`convert`) |
| `config.rs` | `config.toml` の読み込み(どのキーも省略できる) |
| `api.rs` | HTTP(axum)。`/health`、`/romaji`、`/okazuri/search`、`/okazuri/sites` |
| `engine.rs`, `sites.rs`, `query.rs` | 検索エンジン、対象サイトの定義(URL の形、リンクの正規表現)、クエリの整形 |
| `store.rs`, `entity/` | `search.db`(SeaORM / SQLite)。作品、掲載、検索の記録 |
| `mirror.rs`, `d1.rs` | D1 の雑誌のテーブルの写し(`d1-mirror.db`)と、Cloudflare の API |
| `prefetch.rs` | 写しにある雑誌の号を、あらかじめ検索する |
| `cron.rs` | 決めた時刻に HTTP リクエストを送る |
| `romanizer.rs`, `romaji.rs`, `tokenizer.rs`, `utils.rs` | 日本語→ローマ字(lindera + UniDic を実行ファイルに内蔵) |

## 機能とエンドポイント

機能は `config.toml` でオン・オフする。オフの機能のパスは 404。

| パス | 機能 | 内容 |
|---|---|---|
| `GET /health` | 常にオン | 本文 `ok`(`text/plain; charset=utf-8`) |
| `GET /romaji?q=&loanwords=` | 常にオン | 日本語をローマ字にして返す。`q` は 1〜`[convert] max_query_chars`(300)文字。`loanwords` は `romaji`(既定)か `english` |
| `GET /okazuri/search?q=&live=&limit=` | `[search]`(既定オフ) | 各サイトの検索で作品(タイトルと作品ページの path)を集めて保存し、ローマ字にしたクエリの語をすべて含むタイトルの作品を返す |
| `GET /okazuri/sites` | `[search]` | 対象サイトの一覧と、それぞれが今検索されるか |

- 不正なリクエストは 422 と `{"detail": "..."}`。
- JSON の応答は `Content-Type: application/json; charset=utf-8`。
- `live` は `off`(サイトに問い合わせない)、`diff`(既定。未取得か `ttl_hours` を過ぎた組み合わせだけ問い合わせる)、`on`(すべて問い合わせる)。
- 検索では、同時に問い合わせるが、1 つのサイトには `min_delay_ms` より近い間隔で問い合わせない。429 は `Retry-After`(なければ 5 秒、最大 30 秒)の後に 1 回やり直す。Cloudflare のチャレンジが返ったサイトは失敗として扱う。

## サブコマンド

```
okazuri-search-api                       127.0.0.1:8000 でサーバーを起動
okazuri-search-api serve --port 9000     --config <パス> --host <アドレス> --port <ポート>
okazuri-search-api search <語>...        1 回検索(サーバーなし)。--live off|diff|on --limit <n>
okazuri-search-api sync                  D1 の雑誌のテーブルを今すぐ写す
okazuri-search-api prefetch              写しの号を 1 周、事前検索する。--limit <n>
okazuri-search-api convert "文"...       ローマ字を表示。文がなければ標準入力の各行
```

## 設定(`config.toml`、カレントディレクトリ)

`[server]`(`host` 既定 `127.0.0.1`、`port` 既定 8000)、`[search]`(`enabled`、`db_path`、`ttl_hours` 720、`max_query_chars` 300、`timeout_secs`、`min_delay_ms`、`max_results_per_site`、`max_results` 100、`disabled_sites`、`try_unsupported`)、`[d1]`(Cloudflare の D1 の写し。`api_token` は環境変数 `CLOUDFLARE_API_TOKEN`)、`[prefetch]`、`[cron]`(`[[cron.jobs]]`、値の `${NAME}` は環境変数)、`[convert]`、`[exceptions]`。

## データ

- `search.db`: サイトの一覧に出た作品(タイトル、タイトルのローマ字、path)と、どのクエリがどのサイトで何を挙げたかの記録。**新しい作品だけを書き込む**(タイトルが変わったときだけ書き直す)。サイトごとの新しい結果は 1 つのトランザクションで書く。
- `d1-mirror.db`: Worker の D1 の雑誌のテーブルの写し。サーバーが最新に保つ。
- どちらも、なければ作る。

## cron

`[[cron.jobs]]` ごとに、決めた時刻に HTTP リクエストを送る(外部サービス、たとえば Worker の `/internal/fetch-magazines` を起こすため)。実行中に次の時刻が来たジョブは飛ばす。`url` は `http://` か `https://`。

## セキュリティ上の注意

- 既定は `127.0.0.1`。外に出すときはリバースプロキシを前に置く。認証はない。
- `live=on` のリクエストは、外部サイトへの問い合わせを起こす。サイトごとの最小間隔とクエリの長さの上限がある。公開するときは、前段でレート制限を付けること。
- `CLOUDFLARE_API_TOKEN` と cron のヘッダー(`CRON_SECRET` など)は環境変数だけに置き、`config.toml` に書かない(`config.toml` は `.gitignore`)。
- サイトの応答の本文は、`read_to_vec` の上限で読み、リンクは各サイトの正規表現に合うものだけ作品として保存する。
