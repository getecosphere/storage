# photos

Reusable storage domain for any Eco estate. It stores images, documents, and
videos in S3-compatible MinIO; consumers receive opaque object keys, never S3
details. Images are converted to WebP when that is smaller; PDF, Word, Excel,
and text documents are stored as-is; videos are re-encoded to a bandwidth-
friendly MP4 (H.264 + AAC, ≤1280px, ~2 Mbps) with `ffmpeg`.

## Structure

```
photos/
└── backend/     # Rust backend service (Axum + aws-sdk-s3)
```

## Runtime dependency: ffmpeg

`ffmpeg` must be installed on any host running this service (videos are
transcoded with it). It is not a Cargo dependency. Local dev Macs usually have
it; on Eco CTs add `ffmpeg` to the estate's `ecompose.yml` `shared_tools`
(and/or `apt install ffmpeg` before first video upload). Without it, video
uploads fail with `ffmpeg tidak ditemukan`.

## Environment Variables

* `PORT`: Service port (default: 8081)
* `S3_ENDPOINT`: S3 / MinIO endpoint URL
* `S3_REGION`: S3 region
* `S3_BUCKET`: Storage bucket name
* `S3_ACCESS_KEY`: S3 access key
* `S3_SECRET_KEY`: S3 secret key
* `MAX_IMAGE_MB`: Max image upload size in MB (default: 10)
* `MAX_DOCUMENT_MB`: Max document upload size in MB (default: 50)
* `MAX_IMAGE_PIXELS`: Max image pixel count, width × height (default: 40000000)
* `MAX_VIDEO_MB`: Max video upload size in MB (default: 200)

## API

`POST /api/storage/objects` accepts multipart `file`, `owner_id`, `namespace`,
and `reference_id`. Images are converted to WebP only when the converted bytes
are smaller; PDF, Word, Excel, and text documents remain unchanged; videos
(any container ffmpeg understands) are transcoded to MP4 and stored with
`kind: "video"`.

`GET /api/storage/objects?owner_id=&namespace=&reference_id=` lists objects,
`GET /api/storage/content/*key` retrieves content, and `DELETE
/api/storage/objects/*key?owner_id=` removes an owned object.

### Video streaming

`GET /api/storage/content/*key` streams content with HTTP Range support
(`206 Partial Content`, `Accept-Ranges: bytes`, `Content-Range`), so `<video>`
elements start instantly and can seek without downloading the whole object.
HEAD is supported on the same route for probing. A plain GET (no `Range`
header) still returns the full object as `200`.
