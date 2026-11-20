# HTTP API

`200` の応答には、キャッシュを持てる期間が `Cache-Control: public, max-age=60, s-maxage=N` で示されます(検索とカタログの検索は 300 秒、
`/okazuri/sites` は 1 時間、翻訳と `/romaji` は 1 日)。`live=force` の検索、エラー、`/health` は `no-store` です。
すべてのパスは `/api/v1` の下にも置かれています(`/api/v1/okazuri/search`)。デモサーバーは `https://okazuri.nae.quest/api/v1/...`
でも使え(`/api/v1` の後ろは同じパスです)、そこでは Cloudflare が応答をキャッシュします。

`config.toml` でオフにした機能にはルートがなく、そのパスは `404` を返します。不正なリクエストは `422` と
`{"detail": "..."}` を返します。

## `GET /okazuri/search`(`[search]`)

クエリに対して、各サイトが一覧に載せている作品を返します。`live`(既定)では、いまサイトに尋ね、新しい作品を保存してから、
クエリの語をすべてタイトルに含む保存済みの作品(新しい作品も以前に保存した作品も)を返します。詳しくは
[SEARCH.md](SEARCH.md) を見てください。

| クエリ | |
|---|---|
| `q` | 検索語。必須で、1 文字から `[search] max_query_chars` 文字まで(既定 300)。空白で語を区切ります。最初に必ずローマ字にされます。 |
| `live` | `off`: どのサイトにも尋ねず、データベースだけで答えます(古くてもそのまま)。`diff`(既定): 答えがない、または `[search] ttl_hours`(1 か月)より古い、クエリとサイトの組み合わせだけに尋ねます。ほかは `cached` になります。`force`: キャッシュの有無に関係なくすべてのサイトに尋ねます(`searched`。以前の失敗も改めて尋ねます)。どの場合も、新しい作品を保存し、保存済みの作品とあわせてデータベースから答えます |
| `limit` | 応答に入れる作品の最大数。1 から `[search] max_results` まで(既定 100)。 |

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
      "is_legal": true,
      "path": "/books/work/=/product_id/BJ03044883.html",
      "url": "https://www.dlsite.com/books/work/=/product_id/BJ03044883.html",
      "title": "COMIC快楽天 2026年11月号",
      "title_romaji": "comic kairakuten 2026 nen 11 gatsu gou",
      "is_new": true
    }
  ],
  "sites": [
    { "website": "DLsite", "status": "searched", "count": 30, "error": null, "searched_at": 1793181281 }
  ]
}
```

- `title` は、サイトの一覧に載っている作品のタイトルで、サイトが書いたままです。`path` は作品ページのパス(作品を指す
  クエリ文字列のパラメータを含みます)で、`url` はそれにサイトのドメインを付けたものです。
- `input` は送られた `q`(整えたもの)で、`query` はその**ローマ字**です。サイトに尋ねた文字列で、タイトルとの照合にも使います。
  日本語を含まない `q` は、それ自体がローマ字です。`title_romaji` は `title` のローマ字です。
- `title_english` は結果の英語のタイトルで、`{"text": "…", "source": "official" か "google"}` の形です(英語のタイトルがなければ
  `null`)。タイトルが英語ならそのタイトル、両方を併記したタイトル(`English | 日本語`)ならその英語側、そうでなければ保存済みの翻訳です。
  Hitomi.La の翻訳は作品自身のタイトルなので `official`、Google の翻訳は機械翻訳なので `google` です。このために新しくサイトへ尋ねることはありません。
- `translation` は、日本語を含むクエリの英語のタイトルで、`translation_ja` は、日本語を含まないクエリの日本語のタイトルです
  (`{"text", "source"}` または `null`)。そのタイトルを持つ作品も `results` に入ります([SEARCH.md](SEARCH.md))。
- `total` は、すべての語を含む保存済みの作品の数で、`results` は `limit` で切られます。
- `is_new` は、このリクエストがサイトで見つけて保存した作品(それまでデータベースになかった作品)で true になります。
  再び見つかった作品は新しくなく、行もそのままです(タイトルが変わったときだけ書き直します)。
- `sites` は、各サイトの結果を表します。`searched`(いま尋ねた)、`cached`(`live=diff` で、`ttl_hours` 以内に尋ねたので
  データベースが答えた)、`failed`(`error` 付き)、`not searched`(`live=off`)、`unavailable`(Hitomi.La の一覧が全件保存される
  までの間。その間は検索しません)のいずれかです。サイトに尋ねた `queries` も付きます。`q` に日本語があり、
  日本語のタイトルで作品が見つかるサイトでは `["快楽天", "kairakuten"]`、それ以外(日本語を含まない `q`、または日本語のタイトルで
  見つからないサイトの EHentai と HentaiEra)では `["kairakuten"]` です。`count` は、クエリを合わせて一覧に出た作品の数です。
  一部のサイトが失敗しても、リクエストは `200` です。
- 作品はサイトの順に並び、サイトの中では、最後に保存したものが先に来ます。
- `q` がない、または長すぎるとき、および `live` が `off`、`diff`、`force`(または上の別名)でないときは `422` です。

## `GET /okazuri/title`(`[search]`)

作品ページのアドレスから、タイトルを取り出します。BOT の「これらを検索」が使っているのと同じ用途です。まず**保存済みの作品**(`search.db`)から答え、
保存されていない作品だけ、ページを取得してタイトルを読みます。アドレスを `okazuri_bot/src/websites.ts` のパターンに照らして対応するサイトを決め、
次のように作品を探します。

- アドレスを整えます。`#` 以降、`locale` のパラメータ、作品を指さないクエリ文字列(`utm_source` など)を捨て、`/view/1/2` のような閲覧用のアドレスは作品のページに直します。
- 残った path を、末尾の `/` の有無の両方で、保存済みの作品から探します。余計なものが付いたアドレスでも、同じ作品に当たります。
- 見つからなければ、そのページ(Hitomi.La は作品の情報ファイル)を取得し、サイトごとの規則でタイトルを読みます(`<h1>`、`og:title` など。サイトによっては作者名をタイトルの後ろに付けます)。
  Hitomi.La は、保存済みの `English | 日本語` のうち日本語側を返します。

| クエリ | |
|---|---|
| `url` | 作品ページのアドレス。必須で、1 回から 10 回まで繰り返せます(パーセントエンコードしてください)。 |
| `live` | `off`: 保存済みの作品だけで答えます(ページは取得しません)。`diff`(既定): 保存済みの作品、なければページ。`force`: 保存済みの作品を見ず、いつもページを取得します。 |

```
GET /okazuri/title?url=https%3A%2F%2Fwww.dlsite.com%2Fbooks%2Fwork%2F%3D%2Fproduct_id%2FBJ03044883.html%3Futm_source%3Dx&url=https%3A%2F%2Fexample.com%2Fx
{ "total": 2, "results": [
    { "url": "https://www.dlsite.com/books/work/=/product_id/BJ03044883.html?utm_source=x", "website": "DLsite", "title": "COMIC快楽天 2026年11月号",
      "title_japanese": "COMIC快楽天 2026年11月号", "title_romaji": "comic kairakuten 2026 nen 11 gatsu gou", "title_english": null,
      "source": "database", "error": null },
    { "url": "https://example.com/x", "website": null, "title": null, "title_japanese": null, "title_romaji": null, "title_english": null, "source": null, "error": "unsupported website" } ] }
```

結果は、渡した `url` と同じ順です。`title_japanese` は日本語のタイトル(タイトルが日本語ならそれ自身、`English | 日本語` なら日本語側。なければ `null`)、
`title_romaji` はタイトルのローマ字、`title_english` は、検索結果と同じ規則の英語のタイトルとその出どころ(`{text, source}`)です(保存済みのものだけで、このために
どこにも尋ねません)。`source` は `database`(保存済みの作品)か `page`(ページを取得)です。タイトルが取れなかったアドレスは `title` が `null` で、`error` に理由が入ります
(`unsupported website`、`the work is not saved`(`live=off`)、`blocked by a Cloudflare challenge`、`HTTP 404`、`the page has no title` など)。
一部が失敗しても、リクエストは `200` です。`url` がない、11 個以上、`live` が不正なときは `422` です。ページの取得は、検索と同じ `min_delay_ms` を守ります。

保存済みのタイトルは、サイトの一覧に載っていたとおりの文字列なので、ページの `<h1>` と少し違うことがあります。

## `GET /okazuri/sites`(`[search]`)

`okazuri_bot/src/websites.ts` のサイトと、それぞれを検索するかどうかを返します。

```
[ { "name": "DLsite", "is_legal": true, "support": "ready", "has_search": true, "japanese": true, "active": true, "note": "" },
  { "name": "IMHentai", "is_legal": false, "support": "cloudflare", "has_search": true, "japanese": true, "active": false, "note": "" }, ... ]
```

`support` は `ready`、`cloudflare`(ブラウザが要るチャレンジ)、`javascript`、`closed` のどれかです。`japanese` は、そのサイトが
日本語のタイトルで作品を見つけられるかどうかで、`true` なら、日本語を含むクエリを、書かれたとおりの形とローマ字の両方で尋ねます。
`active` は、現在の `[search]` の設定(`disabled_sites`、`try_unsupported`)で、このサーバーがそのサイトを検索するかどうかです。

## カタログの検索(`[magazine]`)

D1 データベースの写し(`[magazine] mirror_path`)から答えます。サイトには尋ねません。写しがない(`[magazine]` がオフで、ファイルもない)ときは、
これらのパスは `404` です。写しはあるが `works` と `authors` がまだ入っていないときは `503` で、`okazuri-search-api sync` を実行するか、
次の自動同期を待ってください(`[magazine] tables` に `authors` と `works` を含めておく必要があります)。作品と号の URL は DLsite のページです。

### `GET /okazuri/magazine`: 作品名から雑誌を探す

作品名(完全一致)が載っている号を、新しい号から返します。BOT の `/magazine` と同じです。同名の作品は複数返ります。

| クエリ | |
|---|---|
| `q` | 作品名。必須で、1 文字から `[romaji] max_query_chars` 文字まで。 |
| `limit` | 最大件数。1 から 50 まで(既定 10)。 |

```
GET /okazuri/magazine?q=作品名
{ "input": "作品名", "total": 1, "results": [ {
    "id": "BJ9", "title": "作品名", "url": "https://www.dlsite.com/books/work/=/product_id/BJ9.html",
    "author": "作者", "position": 4,
    "magazine": { "id": "LBL1", "name": "COMIC快楽天" },
    "issue": { "id": "BJ2", "name": "COMIC快楽天 2026年07月号", "url": "https://www.dlsite.com/books/work/=/product_id/BJ2.html", "date": 1793181281 } } ] }
```

`position` は号の中での位置(1、2、3、…)で、`date` は号の発売日(エポック秒)です。作品がなければ `404`、`q` がない、長すぎる、`limit` が範囲外なら `422` です。

### `GET /okazuri/works`: 号の作品一覧

号の名前(`COMIC快楽天 2026年07月号`。号の id でも可)を完全一致で探し、掲載作品を号の中の順に返します。BOT の `/works` と同じです。

```
GET /okazuri/works?q=COMIC快楽天 2026年07月号
{ "input": "COMIC快楽天 2026年07月号",
  "issue": { "id": "BJ2", "name": "COMIC快楽天 2026年07月号", "url": "...", "date": 1793181281 },
  "magazine": { "id": "LBL1", "name": "COMIC快楽天" },
  "total": 1,
  "results": [ { "id": "BJ9", "title": "作品名", "url": "...", "author": "作者", "position": 4 } ] }
```

号がなければ `404`、`q` がない、または長すぎるときは `422` です。

### `GET /okazuri/magazines`: 雑誌の一覧

写しにあるすべての雑誌を名前順に返します。各雑誌には、号の数 `issues` と、いちばん新しい号 `latest`(号がなければ `null`)が付きます。

```
GET /okazuri/magazines
{ "total": 1, "results": [ { "id": "LBL1", "name": "COMIC快楽天", "issues": 120,
    "latest": { "id": "BJ2", "name": "COMIC快楽天 2026年07月号", "url": "...", "date": 1793181281 } } ] }
```

## `GET /romaji`

日本語の文のローマ字を、`text/plain; charset=utf-8` で返します。

| クエリ | |
|---|---|
| `q` | 文。必須で、1 文字から `[romaji] max_query_chars` 文字まで(既定 300)。 |
| `loanwords` | `romaji`(既定)か `english`。ひらがなで書かれた外来語の書き方です。 |

```
GET /romaji?q=チートアイテム管理局のお仕事
cheat item kanri kyoku no oshigoto

GET /romaji?q=ナカよしふらすとれーしょん！
naka yoshi furasutore-shon!

GET /romaji?q=ナカよしふらすとれーしょん！&loanwords=english
naka yoshi frustration!
```

不正なリクエスト(`q` がない、空、長すぎる、`loanwords` が未知の値)は `422` です。変換できない文は `500` です。

クエリ文字列は UTF-8 でパーセントエンコードする必要があります(JavaScript の `encodeURIComponent`)。Windows のシェルを
通すと日本語が文字化けすることが多いので、エンコードしてくれるプログラムを使ってください。

## `GET /health`

サーバーが待ち受けを始めると `ok` を返します。辞書の読み込みが終わってから待ち受けるので、起動中に接続を
拒否されたときは、まだ準備ができていません。

## 呼び出す側

`okazuri_bot` は `/romaji`(`src/romaji.ts`)を呼んで、検索クエリをローマ字に書き換えていました。`/okazuri/search` は
BOT の Bing 検索の代わりになる検索で、BOT がタイトル(または号の名前)を送ると、それを載せているサイトのページが返ります。

## Python サーバーからの置き換え

`/romaji` は、`okazuri_bot/api` の FastAPI サーバー(cutlet + pykakasi)と同じように答えます。違いは次のとおりです。

- `engine` パラメータはありません。pykakasi のエンジンは、フォールバックにすぎなかったのでなくなりました。
- 辞書は UniDic 2.3.0(unidic-lite)ではなく UniDic 2.1.2(lindera)なので、いくつかの語で区切りや読みが違います。
  どこまで近いかは `docs/ALGORITHM.md` にあります。
