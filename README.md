# photos

Photos domain service for Stuff8 — Handles media upload, presigned URLs, and S3/MinIO storage.

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
* `S3_ACCESS_KEY_ID`: S3 access key
* `S3_SECRET_ACCESS_KEY`: S3 secret key
