# 公開データセット

保存した作品の一覧を、研究などのために誰でも取れる形で配るための仕組みです。権利者が違法アップロードを見つけ、削除要請などの対応に
進むまでの時間を短くすることも、目的のひとつです。

```
okazuri-search-api dataset <ディレクトリ> [--config <パス>]
```

ディレクトリに次の 2 つを書きます(一時ファイルに書いてから名前を変えるので、読み手に書きかけのファイルは見えません)。

- `works.jsonl.gz`: 作品を 1 行に 1 つ、JSON で書いて gzip で圧縮したファイル。
- `meta.json`: 作った日時、作品の数、サイトごとの数、項目の意味、注意書き。

## 1 行の項目

| 項目 | 意味 |
|---|---|
| `site` | 作品を載せているサイトの名前。 |
| `is_legal` | `true`: 正規(ライセンス済み)のサイト。`false`: 違法アップロードで知られるサイト。 |
| `url` | 作品のページのアドレス。 |
| `title` | サイトに書かれているままのタイトル。 |
| `language` | タイトルが示す言語の 2 文字コードの配列(なければ空)。 |
| `first_seen` | このクローラーが**初めて保存した**時刻(Unix 秒)。**作品がアップロード・公開された日ではありません。** |
| `checked_at` | ページがまだあるかを最後に確かめた時刻(Unix 秒)。確かめていなければ `null`。 |

- 載るのは、書き出したときに一覧に載っていた作品だけです。ページが消えた作品は含みません。
- クローラーが見つけた作品の一覧で、サイトにある作品の全部ではありません。
- `first_seen` と `checked_at` は、アップロードの時期や削除要請の証拠にはなりません。
- 検索履歴、ローマ字、翻訳、利用者に関するものは入りません。未知のサイトの作品も入りません。

## 毎週の配布(R2)

最新版だけを、毎週同じ名前で上書きします。置き場所は Cloudflare R2 の公開バケットです(送信料がかからず、最低保存期間もありません)。

1. R2 のバケット(例: `okazuri-dataset`)を作り、公開アクセス(または独自ドメイン)を有効にします。
2. R2 の API トークン(そのバケットの読み書き)を作り、サーバーの rclone に `r2` という名前のリモートを設定します
   (`rclone config`。種類は S3、プロバイダーは Cloudflare R2)。
3. `tools/dataset/` のファイルを置きます。

```
sudo install -m 755 tools/dataset/publish.sh /usr/local/bin/okazuri-dataset-publish
sudo install -m 644 tools/dataset/okazuri-dataset.service tools/dataset/okazuri-dataset.timer /etc/systemd/system/
sudo tee /etc/okazuri-dataset.env <<'ENV'
DATASET_REMOTE=r2:okazuri-dataset
OKAZURI_BIN=/usr/local/bin/okazuri-search-api
OKAZURI_CONFIG=/etc/okazuri/config.toml
ENV
sudo systemctl daemon-reload
sudo systemctl enable --now okazuri-dataset.timer
sudo systemctl start okazuri-dataset.service   # 1 回すぐ試す
```

`works.jsonl.gz` と `meta.json`、チェックサム(`works.jsonl.gz.sha256`)が置かれます。サーバーが動いたままでも書き出せます。
