# okazuri-search-api 仕様

[オカズリBOT](https://github.com/NaeCqde/okazuri_bot) のための検索サーバー。1 つの実行ファイルに、日本語→ローマ字変換、雑誌・作品の検索、Cloudflare D1 の写し、事前検索、定期リクエスト(cron)が入っている。
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
| `GET /romaji?q=&loanwords=` | 常にオン | 日本語をローマ字にして返す。`q` は 1〜`[romaji] max_query_chars`(300)文字。`loanwords` は `romaji`(既定)か `english` |
| `GET /okazuri/search?q=&live=&limit=` | `[search]`(既定オフ) | 各サイトの検索で作品(タイトルと作品ページの path)を集めて保存し、ローマ字にしたクエリの語をすべて含むタイトルの作品を返す |
| `GET /okazuri/sites` | `[search]` | 対象サイトの一覧と、それぞれが今検索されるか |

- 不正なリクエストは 422 と `{"detail": "..."}`。
- JSON の応答は `Content-Type: application/json; charset=utf-8`。
- `live` は `off`(サイトに問い合わせない)、`diff`(既定。未取得か `ttl_hours` を過ぎた組み合わせだけ問い合わせる)、`force`(すべて問い合わせる)。
- 検索では、同時に問い合わせるが、1 つのサイトには `min_delay_ms` より近い間隔で問い合わせない。429 は `Retry-After`(なければ 5 秒、最大 30 秒)の後に 1 回やり直す。Cloudflare のチャレンジが返ったサイトは失敗として扱う。

## サブコマンド

```
okazuri-search-api                       設定ファイルのポートでサーバーを起動
okazuri-search-api serve --port 9000     --config <パス> --host <アドレス> --port <ポート>
okazuri-search-api search <語>...        1 回検索(サーバーなし)。--live off|diff|force --limit <n>
okazuri-search-api title <URL>...         ページのタイトルと、日本語名・ローマ字・英訳を表示する。--live off|diff|force
okazuri-search-api magazines             雑誌の一覧と最新号を表示する
okazuri-search-api magazine <題名>       作品が載っている号を表示する。--limit <n>
okazuri-search-api works <号>            号の作品一覧を表示する
okazuri-search-api sync                  D1 の雑誌のテーブルを今すぐ写す
okazuri-search-api prefetch              写しの号を 1 周、事前検索する。--limit <n>
okazuri-search-api convert "文"...       ローマ字を表示。文がなければ標準入力の各行
```

## 設定(`config.toml`、カレントディレクトリ)

`[server]`(`host` 既定 `127.0.0.1`、`port` 既定 8000)、`[search]`(`enabled`、`db_path`、`ttl_hours` 720、`max_query_chars` 300、`timeout_secs`、`min_delay_ms`、`max_results_per_site`、`max_results` 100、`disabled_sites`)、`[cloudflare]`(`enabled`、`api`、`cache_minutes` 30。チャレンジの向こうのサイトを読むための、クッキー・User-Agent・proxy を返すサーバー)、`[magazine]`(Cloudflare の D1 の写し。`api_token` は環境変数 `CLOUDFLARE_API_TOKEN`。事前検索は `[magazine.prefetch]`)、`[cron]`(`[[cron.jobs]]`、値の `${NAME}` は環境変数)、`[romaji]`(`max_query_chars`、`[romaji.exceptions]`)。

## 全件読み直し(`rewalk` コマンド)

あとから取れるようになった項目(タグなど)を、保存済みの作品に入れ直すための読み直し。`okazuri-search-api rewalk Wnacg Sukebei` のように、サイト名(大文字小文字は区別しない)を並べて実行する。そのサイトと各シャード(`Site#k/N`)の、読み取り済みの印(`crawls.done`)とカーソルを戻し、読んだページの範囲(`crawl_ranges`)を消す。保存済みの作品は消えず、ページの今の内容(タグ・投稿日など)で更新される。テキスト索引は開かず、DB だけを開くので、サーバーが動いている間も実行できる。ただしサーバーを再起動したときに読み直しが始まる(読み終えたレーンは終了しているため)。何度でも実行できる。

## ギャップチェック(`[crawl]` の `gap_check`、`watch_seconds` など)

新しい順に並ぶ番号つきの一覧(HentaiEnvy、HentaiEra、AsmHentai、Wnacg、Hitomi.La)で、最初の歩きの間に先頭へ作品が増えると、全ページが 1 作品ぶん下へずれます。
ワーカーに分けると、ページは順不同に読まれます。**ページ k+1 のほうがページ k より先に読まれ、そのあいだに先頭が増える**と、k の末尾から k+1 の先頭へ移った作品は、どちらにも読まれません(ギャップ)。
順に読んだ(k が先)場合は、同じ作品を 2 回読むだけで、害はありません。

- **対象:** `numbered(source) && !can_oldest(source)`、つまりホスト(`[distribute] role = "host"`)が `[crawl] gap_check = true` で歩く、新しい順の番号つき一覧。
  古い順に読める一覧(WordPress の各サイト、DLsite)はページが動かないので、行も監視も作らない。カーソルで 1 つずつ読む一覧(RokuHentai、E-Hentai、FANZA、JComic、Sukebei、同人の森)も順に読むので対象外。
  `role` が `off` と `worker` のときと、`done` のサイトも対象外。Hitomi.La は新しいギャラリーがインデックスの先頭(オフセット 0)に入るので対象。
- **読んだ時刻:** ジョブごとに、ホストがワーカーに渡した時刻(`taken`)、答えを受けた時刻、ワーカーが数えたページごとの経過ミリ秒(`Step.at`)から、各ページを読んだ最早と最遅の時刻を `crawl_reads` に、ページの保存と同じトランザクションで書く
  (ワーカーの時計は使わないので、マシン間のずれは影響しない。`at` がない古いワーカーのページは、ジョブ全体の時間のどこかで読んだことにする)。
- **先頭の監視:** 歩きが終わるまで、ホストは `watch_seconds`(既定 120 秒。サイトの間隔の 4 倍より短くしない。0 で監視しない)ごとに 1 ページ目を自分で読み、前回との差で増えた作品の数を `crawl_growth` に、
  増えたときだけ書く(`crawl_watch` は最後の見た目。Hitomi.La はインデックスのバイト数、`Content-Range` から)。最初の 1 回は基準点で、最初のジョブより前に行う。BAN 中は読まない。
- **境界の判定:** 境界(k, k+1)は、別のジョブが読み、k+1 を読んだ最早の時刻が k を読んだ最遅の時刻より前で、**かつ**その間(k+1 の最早〜k の最遅)に先頭が増えたか、その時間を監視していない(基準点より前、最後の監視より後、`watch_seconds = 0`)ときに危険とする。
- **読み直し:** 歩きが終わると、監視を 1 回してから、危険な境界ごとに k+1 から下へ n ページ(n は k+1 を読んだあとの増加から、`ceil((増加 + 1 ページの半分) / 1 ページの作品数)`、監視していない・飽和したときは `gap_reread_max_pages`)を、上から順に読み直す。
  重なる範囲はまとめ、`gap_budget_pages` を超えるぶんは増加の大きい順に残して、残りは警告して更新(リフレッシャー)に任せる。計画は 1 回だけ作って `crawl_gap_plan` に残し、再起動してもその行から続ける。
  1 つの範囲は 3 回(`GAP_TRIES`)失敗すると諦めて(エラーログ)、残りを先へ進める。読み直しは新しい境界を作らず、増加が続いても計画は変わらないので、必ず終わる。`mark_crawled` は計画が終わってから(1 つのトランザクションで `crawl_reads`、`crawl_watch`、`crawl_growth`、`crawl_gap_plan` の行を消す)。
- **ID のチェック:** HentaiEnvy、HentaiEra、AsmHentai、Wnacg(`id_gap_sites`)は作品の ID が連番なので、一覧を読み終えたあとの更新のたびに、保存した最小と最大の ID のあいだで欠けているもの(新しいほうから、1 回に `id_gap_per_cycle` 個)を調べる。
  ページが消えていれば `crawl_probes` に記録して二度と尋ねず、あれば読んで保存する。尋ねた結果が分からなかった(エラー、BAN)ときは何も記録しない。ページが消えたことを示す規則(`gone_rule`)がないサイトは対象にできない(E-Hentai はギャラリーの URL にトークンが要るので外す。Hitomi.La、Sukebei、DLsite は使うなら `id_gap_sites` に書く)。
- **既知の限界:** 先頭近くの作品が**削除**されると、一覧が上へずれて、順に読んでも作品を 1 つ読み落とすことがある。これを見つけるのは ID のチェックだけ。
- **費用の上限:** 監視は 1 サイトにつき `max(watch_seconds, 4 × 間隔)` ごとに 1 リクエスト、読み直しは 1 回の歩きに最大 `gap_budget_pages` ページ、ID のチェックは更新 1 回に最大 `id_gap_per_cycle` リクエスト。テーブルはサイトごとの小さな行で、メモリに載せる大きな構造はない
  (ID のチェックだけ、保存した ID を `u32` で並べる)。
- **互換:** `crawl_reads` に行がないページ(この変更より前の歩きで読んだもの)には境界がなく、安全として扱う(コードに `compat` と書いてあり、本番の移行が終わったら消す)。

## データ

- `search.db`: サイトの一覧に出た作品(タイトル、タイトルのローマ字、path。一覧ページに出ていれば、タグ(名前空間なしの平らなもの。E-Hentai のカテゴリもタグ)、投稿者、投稿日と、それらのローマ字)と、どのクエリがどのサイトで何を挙げたかの記録。**新しい作品だけを書き込む**(タイトルが変わったときだけ書き直す)。サイトごとの新しい結果は 1 つのトランザクションで書く。
- `d1-mirror.db`: Worker の D1 の雑誌のテーブルの写し。サーバーが最新に保つ。
- どちらも、なければ作る。

## cron

`[[cron.jobs]]` ごとに、決めた時刻に HTTP リクエストを送る(外部サービス、たとえば Worker の `/internal/fetch-magazines` を起こすため)。実行中に次の時刻が来たジョブは飛ばす。`url` は `http://` か `https://`。

## セキュリティ上の注意

- 既定は `127.0.0.1`。外に出すときはリバースプロキシを前に置く。認証はない。
- `live=force` のリクエストは、外部サイトへの問い合わせを起こす。サイト(Google Translate を含む)ごとの最小間隔とクエリの長さの上限がある。公開するときは、前段でレート制限を付けること。
- `CLOUDFLARE_API_TOKEN` と cron のヘッダー(`CRON_SECRET` など)は環境変数だけに置き、`config.toml` に書かない(`config.toml` は `.gitignore`)。
- サイトの応答の本文は、`read_to_vec` の上限で読み、リンクは各サイトの正規表現に合うものだけ作品として保存する。
