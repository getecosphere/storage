#!/usr/bin/env bash
# storage LXS smoke test — golden request→response pairs.
# Usage: BASE_URL=<http://host:port/api> ./examples.sh
# Runs against a pulled binary or a live estate URL; every curl must succeed
# and return the documented shape or the script exits non-zero.
#
# Uploads a real 1x1 PNG (tiny, decodes fine, WebP re-encode + thumbnail
# generation kick in), fetches it back, lists it, and deletes it.
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:8081/api}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# 1x1 transparent PNG (valid; image decodes, WebP re-encode + thumb apply)
printf 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==' \
  | base64 -d > "$tmp/pixel.png"

OWNER="smoke-$(date +%s)"

# 1) health
code=$(curl -s -o "$tmp/health.out" -w '%{http_code}' "$BASE_URL/health")
test "$code" = "200"
grep -q "OK - Storage domain service" "$tmp/health.out"
echo "OK /health -> 200"

# 2) upload an image (multipart) -> 201 with key + thumbnail_key
code=$(curl -s -o "$tmp/upload.out" -w '%{http_code}' \
  -F "file=@$tmp/pixel.png;type=image/png" \
  -F "owner_id=$OWNER" -F "namespace=smoke" -F "reference_id=ref-1" \
  "$BASE_URL/storage/objects")
test "$code" = "201"
key=$(python3 -c "import json;print(json.load(open('$tmp/upload.out'))['key'])")
thumb=$(python3 -c "import json;print(json.load(open('$tmp/upload.out'))['thumbnail_key'])")
test -n "$key" && test -n "$thumb"
echo "OK POST /storage/objects -> 201 (key=$key, thumbnail_key=$thumb)"

# 3) fetch content by key -> 200
code=$(curl -s -o "$tmp/content.out" -w '%{http_code}' "$BASE_URL/storage/content/$key")
test "$code" = "200"
echo "OK GET /storage/content/\$key -> 200"

# 4) fetch the thumbnail -> 200 (image uploads always produce a -thumb.webp)
code=$(curl -s -o "$tmp/thumb.out" -w '%{http_code}' "$BASE_URL/storage/content/$thumb")
test "$code" = "200"
echo "OK GET /storage/content/\$thumbnail_key -> 200"

# 5) object metadata by key -> 200 with matching key
code=$(curl -s -o "$tmp/meta.out" -w '%{http_code}' "$BASE_URL/storage/objects/$key")
test "$code" = "200"
meta_key=$(python3 -c "import json;print(json.load(open('$tmp/meta.out'))['key'])")
test "$meta_key" = "$key"
echo "OK GET /storage/objects/\$key -> 200"

# 6) list owned objects -> 200, our key present
code=$(curl -s -o "$tmp/list.out" -w '%{http_code}' \
  "$BASE_URL/storage/objects?owner_id=$OWNER&namespace=smoke&reference_id=ref-1")
test "$code" = "200"
grep -q "$key" "$tmp/list.out"
echo "OK GET /storage/objects?... -> 200 (contains key)"

# 7) delete by key with owner_id -> 204
code=$(curl -s -o "$tmp/delete.out" -w '%{http_code}' -X DELETE \
  "$BASE_URL/storage/objects/$key?owner_id=$OWNER")
test "$code" = "204"
echo "OK DELETE /storage/objects/\$key?owner_id=... -> 204"

# 8) content fetch after delete -> 404
code=$(curl -s -o "$tmp/gone.out" -w '%{http_code}' "$BASE_URL/storage/content/$key")
test "$code" = "404"
echo "OK GET deleted content -> 404"

echo "ALL STORAGE SMOKE TESTS PASSED"
