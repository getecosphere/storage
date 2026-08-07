# photos

Reusable storage domain for any Eco estate. It stores images and documents in
S3-compatible MinIO; consumers receive opaque object keys, never S3 details.

## Structure

```
photos/
└── backend/     # Rust backend service (Axum + aws-sdk-s3)
```

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

## API

`POST /api/storage/objects` accepts multipart `file`, `owner_id`, `namespace`,
and `reference_id`. Images are converted to WebP only when the converted bytes
are smaller; PDF, Word, Excel, and text documents remain unchanged.

`GET /api/storage/objects?owner_id=&namespace=&reference_id=` lists objects,
`GET /api/storage/content/*key` retrieves content, and `DELETE
/api/storage/objects/*key?owner_id=` removes an owned object.
