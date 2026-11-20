#!/bin/sh
# Writes the public dataset and uploads it to an R2 bucket (rclone remote). Run weekly by okazuri-dataset.timer.
#
#   OKAZURI_BIN      the executable            (default: okazuri-search-api)
#   OKAZURI_CONFIG   its config file           (default: config.toml in the current directory)
#   DATASET_REMOTE   rclone destination        (e.g. r2:okazuri-dataset), required
#   DATASET_README   the README to upload too  (default: none)
set -eu

: "${DATASET_REMOTE:?DATASET_REMOTE is required, e.g. r2:okazuri-dataset}"
BIN="${OKAZURI_BIN:-okazuri-search-api}"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

"$BIN" dataset "$OUT" ${OKAZURI_CONFIG:+--config "$OKAZURI_CONFIG"}

# a checksum next to the data, so a reader can check the download
( cd "$OUT" && sha256sum works.jsonl.gz > works.jsonl.gz.sha256 )
[ -n "${DATASET_README:-}" ] && cp "$DATASET_README" "$OUT/README.md"

# only the latest is kept: the same names every week. The data goes first and meta.json last, so meta.json is never newer than its data
rclone copyto "$OUT/works.jsonl.gz" "$DATASET_REMOTE/works.jsonl.gz"
rclone copyto "$OUT/works.jsonl.gz.sha256" "$DATASET_REMOTE/works.jsonl.gz.sha256"
[ -f "$OUT/README.md" ] && rclone copyto "$OUT/README.md" "$DATASET_REMOTE/README.md"
rclone copyto "$OUT/meta.json" "$DATASET_REMOTE/meta.json"
