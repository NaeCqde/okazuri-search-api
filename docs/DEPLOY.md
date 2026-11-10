# Running it on a server

For a Linux x86_64 VPS or container use the **musl** archive (`okazuri-search-api-Linux-x86_64-musl`): the
executable is static, so it needs no glibc and no other file. It is about 190 MB because UniDic is inside, but
the dictionary is mapped from the file and only the parts that are used are read: measured on Windows, it
starts in 0.3 s and uses about 8 MB of memory after 300 requests (the operating system may keep the whole
file in its cache).

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
# the log level: error, warn, info (default), debug
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

The server listens only after the dictionary is loaded, so right after a start the port is not open yet.
`systemctl stop` sends SIGTERM, which ends it cleanly.

An optional `config.toml` is read from the working directory (give the service a `WorkingDirectory=`), or
named with `--config`.

## Scheduled requests (`[cron]`)

The server can send HTTP requests on a schedule, for a service that has to be called from this machine. It is off
until `[cron]` has `enabled = true` in `config.toml`:

```toml
[cron]
enabled = true
utc_offset = "+09:00"            # the clock the schedules are read on (default UTC)

[[cron.jobs]]
name = "okazuri-fetch-magazines"
schedule = "*/10 * * * *"        # minute hour day-of-month month day-of-week
method = "POST"
url = "https://okazuri-bot.example.workers.dev/internal/fetch-magazines"
timeout_secs = 150               # the Worker answers when it has done what it can in about a minute

[cron.jobs.headers]
Authorization = "Bearer ${CRON_SECRET}"   # the environment variable CRON_SECRET
```

- The secret is read from the environment, so with systemd put it in `EnvironmentFile=` (a file only the service
  user can read), not in `config.toml`. A variable that is not set stops the start.
- A mistake in the section (a schedule, a URL, a missing variable) stops the start with a message that names the
  job, before the dictionary is loaded.
- Each minute every job is asked whether its schedule matches. A job that is still running when its next time comes
  is skipped (`timeout_secs`, 30 by default, ends a request that hangs). The result of each request, its status
  and the start of the answer, is written to the log; a status that is not 2xx is a warning.
- The schedule is read on `utc_offset`, which is a fixed offset: there is no daylight saving time.
- Requests are not retried and a minute that is missed (the machine was asleep) is not made up for.

## The search engine

`[search]` keeps its results in a SQLite file (`search.db`), and `[d1]` keeps its copy of the magazine tables in
another (`d1-mirror.db`). Both are made if they are not there, relative to the working directory, so give the
service a `WorkingDirectory=` that it may write to. The SQLite library is inside the executable (SeaORM, with sqlx, reads and
writes both files).

```toml
[search]
enabled = true

[d1]
enabled = true
account_id = "<your Cloudflare account id>"
database_id = "<the id of the D1 database>"
# api_token is read from the environment variable CLOUDFLARE_API_TOKEN (an API token that can edit D1)

[prefetch]
enabled = true
```

```ini
# the unit of systemd: the token in a file only the service user can read
EnvironmentFile=/etc/okazuri-search-api.env       # CLOUDFLARE_API_TOKEN=...
WorkingDirectory=/var/lib/okazuri-search-api
```

- `okazuri-search-api sync` copies the tables once and says whether the data changed; use it to check the token.
- `okazuri-search-api search <words>` searches once from the command line, and `prefetch --limit 5` runs a few queries
  of the prefetch.
- The sites are asked from this machine, so a VPS in a datacenter is seen as one by the sites that block those. The
  ones that answer with a Cloudflare challenge (`GET /okazuri/sites`) are left out.
- The first prefetch round searches every issue of the copy on every site, about a thousand queries, politely
  (`min_delay_ms`, `query_delay_ms`): it takes hours, and goes on where it stopped after a restart.

## In front of it

The default address `127.0.0.1` is on purpose: the server has no authentication and no rate limit. To open it up
put a reverse proxy in front, and limit what it does there. nginx:

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

The static executable runs in an empty image:

```dockerfile
FROM scratch
COPY okazuri-search-api /okazuri-search-api
EXPOSE 8000
ENTRYPOINT ["/okazuri-search-api", "serve", "--host", "0.0.0.0"]
```

`--host 0.0.0.0` is needed in a container; publish the port to `127.0.0.1` only unless something in front of it
protects it (`-p 127.0.0.1:8000:8000`).
