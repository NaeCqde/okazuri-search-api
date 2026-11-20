# サーバーで動かす

Linux の VPS やコンテナでは、**musl** の実行ファイル(x86_64 は `okazuri-search-api-Linux-x86_64-musl`、ARM は `okazuri-search-api-Linux-aarch64-musl`)を使います。実行ファイルは静的なので、
glibc も、ほかのファイルも要りません。UniDic が中に入っているため約 190 MB ありますが、辞書はファイルからメモリに対応付けられ、
使う部分だけが読み込まれます。Windows で測ったところ、起動は 0.3 秒で、300 回のリクエストの後でも使用メモリは約 8 MB でした
(OS がファイル全体をキャッシュに残すことはあります)。

## systemd

```
sudo install -m 755 okazuri-search-api /usr/local/bin/okazuri-search-api
sudo useradd --system --no-create-home romaji
```

`/etc/systemd/system/okazuri-search-api.service`:

```ini
[Unit]
Description=Okazuri Search API
After=network.target

[Service]
User=romaji
ExecStart=/usr/local/bin/okazuri-search-api serve --host 127.0.0.1 --port 8000
Restart=on-failure
# ログのレベル: error、warn、info(既定)、debug
Environment=RUST_LOG=info
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

```
sudo systemctl daemon-reload
sudo systemctl enable --now okazuri-search-api
curl http://127.0.0.1:8000/health
```

サーバーが待ち受けを始めるのは辞書を読み込んだ後なので、起動した直後はまだポートが開いていません。
`systemctl stop` は SIGTERM を送り、サーバーはそれできれいに終了します。

任意の `config.toml` は、作業ディレクトリから読みます(サービスに `WorkingDirectory=` を指定してください)。`--config` で指定することもできます。

## 定期リクエスト(`[cron]`)

サーバーは、このマシンから呼ぶ必要があるサービスのために、決まった時刻に HTTP リクエストを送れます。`config.toml` の `[cron]` で
`enabled = true` にするまでは、オフです。

```toml
[cron]
enabled = true
utc_offset = "+09:00"            # スケジュールを読む時計(既定は UTC)

[[cron.jobs]]
name = "okazuri-fetch-magazines"
schedule = "*/10 * * * *"        # 分 時 日 月 曜日
method = "POST"
url = "https://okazuri-bot.example.workers.dev/internal/fetch-magazines"
timeout_secs = 150               # Worker は、約 1 分でできることをやり終えたら応答します

[cron.jobs.headers]
Authorization = "Bearer ${CRON_SECRET}"   # 環境変数 CRON_SECRET
```

- 秘密の値は環境変数から読みます。systemd では、`config.toml` ではなく `EnvironmentFile=`(サービスのユーザーだけが読めるファイル)に
  置いてください。設定されていない変数があると、起動が止まります。
- セクションの誤り(スケジュール、URL、足りない変数)があると、辞書を読み込む前に、ジョブの名前を挙げたメッセージを出して起動が止まります。
- 毎分、すべてのジョブについて、スケジュールが合うかどうかを調べます。次の時刻が来ても前の実行が終わっていないジョブは、飛ばされます
  (`timeout_secs`。既定は 30 秒で、止まったままのリクエストを終わらせます)。各リクエストの結果(ステータスと応答の先頭)はログに書かれ、
  2xx でないステータスは警告になります。
- スケジュールは `utc_offset` で読みます。これは固定のオフセットで、夏時間はありません。
- リクエストは再試行されず、逃した分(マシンが止まっていたとき)の埋め合わせもしません。

## 検索エンジン

`[search]` は結果を SQLite ファイル(`search.db`)に、`[d1]` は雑誌のテーブルの写しを別のファイル(`d1-mirror.db`)に保存します。
どちらも、なければ作業ディレクトリの下に作られるので、サービスには、書き込める `WorkingDirectory=` を指定してください。
SQLite のライブラリは実行ファイルに入っています(SeaORM が sqlx を使って、両方のファイルを読み書きします)。

```toml
[search]
enabled = true

[d1]
enabled = true
account_id = "<Cloudflare のアカウント ID>"
database_id = "<D1 データベースの ID>"
# api_token は環境変数 CLOUDFLARE_API_TOKEN から読む(D1 を編集できる API トークン)

[prefetch]
enabled = true
```

```ini
# systemd のユニット: トークンはサービスのユーザーだけが読めるファイルに
EnvironmentFile=/etc/okazuri-search-api.env       # CLOUDFLARE_API_TOKEN=...
WorkingDirectory=/var/lib/okazuri-search-api
```

- `okazuri-search-api sync` はテーブルを 1 回写し、データが変わったかどうかを表示します。トークンの確認に使えます。
- `okazuri-search-api search <語>` はコマンドラインから 1 回検索します。`prefetch --limit 5` は事前検索のクエリを数件だけ実行します。
- サイトへの問い合わせはこのマシンから出るため、データセンターの VPS は、それを遮断するサイトからは、そのものとして見られます。
  Cloudflare のチャレンジで応答するサイト(`GET /okazuri/sites`)は、対象から外します。
- 最初の事前検索では、写しのすべての号を、すべてのサイトで検索します(約 1000 クエリ)。礼儀正しく間隔をあけるため
  (`min_delay_ms`、`query_delay_ms`)、数時間かかり、再起動しても止まったところから続けます。

## 前に置くもの

既定のアドレス `127.0.0.1` は意図したものです。サーバーには認証もレート制限もありません。公開するときは、リバースプロキシを前に置き、
そこで許可する内容を絞ってください。nginx の例です。

```nginx
limit_req_zone $binary_remote_addr zone=romaji:10m rate=10r/s;

server {
    listen 443 ssl;
    server_name romaji.example.com;
    # ssl_certificate ...

    location = /romaji {
        limit_req zone=romaji burst=20 nodelay;
        proxy_pass http://127.0.0.1:8000;
    }
    location = /health {
        proxy_pass http://127.0.0.1:8000;
    }
    location / { return 404; }
}
```

## Docker

静的な実行ファイルは、空のイメージで動きます。

```dockerfile
FROM scratch
COPY okazuri-search-api /okazuri-search-api
EXPOSE 8000
ENTRYPOINT ["/okazuri-search-api", "serve", "--host", "0.0.0.0"]
```

コンテナの中では `--host 0.0.0.0` が必要です。前に置いたものが守ってくれない限り、ポートは `127.0.0.1` にだけ公開してください
(`-p 127.0.0.1:8000:8000`)。
