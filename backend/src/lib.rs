use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::{config::Region, primitives::ByteStream, Client};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, GenericImageView};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::io::ReaderStream;
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
};
use uuid::Uuid;
use webp::Encoder;

#[derive(Debug, Clone, Copy)]
pub struct UploadLimits {
    pub max_image_bytes: usize,
    pub max_document_bytes: usize,
    pub max_image_pixels: u32,
    pub max_video_bytes: usize,
    pub image_max_dimension: u32,
    pub thumbnail_dimension: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredObject {
    pub key: String,
    pub original_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub kind: String,
    pub owner_id: String,
    pub namespace: String,
    pub reference_id: String,
    pub created_at: DateTime<Utc>,
    pub thumbnail_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListObjectsQuery {
    pub owner_id: String,
    pub namespace: String,
    pub reference_id: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteObjectQuery {
    pub owner_id: String,
}

#[derive(Clone)]
pub struct AppState {
    pub client: Client,
    pub bucket: String,
    pub limits: UploadLimits,
}

pub struct PhotosService {
    pub state: AppState,
    pub port: u16,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/api/photos/health", get(health_check))
        .route(
            "/api/storage/objects",
            post(upload_object).get(list_objects),
        )
        .route(
            "/api/storage/content/*key",
            get(download_object).head(download_headers),
        )
        .route(
            "/api/storage/objects/*key",
            get(object_metadata).delete(delete_object),
        )
        .layer(RequestBodyLimitLayer::new(max_body_bytes(state.limits)))
        .layer(DefaultBodyLimit::max(max_body_bytes(state.limits) + 1024 * 1024))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state)
}

pub fn env(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

pub fn env_u64(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(fallback)
}

pub fn max_body_bytes(limits: UploadLimits) -> usize {
    limits
        .max_image_bytes
        .max(limits.max_document_bytes)
        .max(limits.max_video_bytes)
}

fn required_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be configured for the storage domain"))
}

pub async fn ensure_bucket(state: &AppState) -> Result<(), aws_sdk_s3::Error> {
    if state
        .client
        .head_bucket()
        .bucket(&state.bucket)
        .send()
        .await
        .is_err()
    {
        state
            .client
            .create_bucket()
            .bucket(&state.bucket)
            .send()
            .await?;
    }
    Ok(())
}

async fn health_check() -> &'static str {
    "OK - Storage domain service (S3/MinIO)"
}

async fn upload_object(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<StoredObject>), (StatusCode, String)> {
    let mut fields = HashMap::new();
    let mut filename = None;
    let mut content_type = None;
    let mut bytes = None;
    let mut video_input = None;

    while let Some(field) = multipart.next_field().await.map_err(bad_request)? {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "file" {
            filename = field.file_name().map(ToOwned::to_owned);
            content_type = field.content_type().map(ToOwned::to_owned);
            let declared = content_type
                .clone()
                .unwrap_or_else(|| mime_from_name(filename.as_deref().unwrap_or("upload")));
            if is_video(&declared) {
                let mut field = field;
                let path = std::env::temp_dir().join(format!(
                    "eco-video-in-{}.{}",
                    Uuid::new_v4(),
                    temp_ext(&declared)
                ));
                let mut file = tokio::fs::File::create(&path).await.map_err(bad_request)?;
                while let Some(chunk) = field.chunk().await.map_err(bad_request)? {
                    file.write_all(&chunk).await.map_err(bad_request)?;
                }
                file.flush().await.map_err(bad_request)?;
                drop(file);
                video_input = Some(path);
            } else {
                bytes = Some(field.bytes().await.map_err(bad_request)?.to_vec());
            }
        } else {
            fields.insert(name, field.text().await.map_err(bad_request)?);
        }
    }

    let owner_id = safe_segment(required_field(&fields, "owner_id")?)?;
    let namespace = safe_segment(required_field(&fields, "namespace")?)?;
    let reference_id = safe_segment(required_field(&fields, "reference_id")?)?;
    let original_name = filename.unwrap_or_else(|| "upload".to_string());
    let declared_type = content_type.unwrap_or_else(|| mime_from_name(&original_name));

    if bytes.is_none() && video_input.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Field file wajib diisi".to_string(),
        ));
    }

    let (put_body, mime_type, extension, kind, size_bytes, output_to_clean, thumbnail_bytes) =
        if let Some(input_path) = video_input {
            let transcoded = transcode_video(state.limits, &input_path, &original_name).await?;
            let _ = tokio::fs::remove_file(&input_path).await;
            let body = ByteStream::from_path(&transcoded.path)
                .await
                .map_err(s3_error)?;
            (
                body,
                transcoded.mime_type,
                transcoded.extension,
                transcoded.kind,
                transcoded.size_bytes,
                Some(transcoded.path),
                None,
            )
        } else {
            let processed = process_upload(
                state.limits,
                bytes.as_deref().unwrap_or_default(),
                &declared_type,
                &original_name,
            )?;
            (
                ByteStream::from(processed.bytes),
                processed.mime_type,
                processed.extension,
                processed.kind,
                processed.size_bytes as u64,
                None,
                processed.thumbnail,
            )
        };
    let object_id = Uuid::new_v4();
    let key = format!(
        "{namespace}/{reference_id}/{}.{}",
        object_id, extension
    );
    let now = Utc::now();

    let put_metadata = |builder: aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder| {
        builder
            .metadata("owner-id", &owner_id)
            .metadata("namespace", &namespace)
            .metadata("reference-id", &reference_id)
            .metadata("original-name", &original_name)
            .metadata("kind", kind)
            .metadata("created-at", now.to_rfc3339())
    };

    let request = put_metadata(
        state
            .client
            .put_object()
            .bucket(&state.bucket)
            .key(&key)
            .body(put_body)
            .content_type(&mime_type),
    );
    request.send().await.map_err(s3_error)?;

    let thumbnail_key = thumbnail_bytes.as_ref().map(|_| {
        format!(
            "{namespace}/{reference_id}/{object_id}-thumb.webp"
        )
    });
    if let (Some(thumb), Some(thumb_key)) = (thumbnail_bytes.as_ref(), thumbnail_key.as_ref()) {
        let thumb_request = put_metadata(
            state
                .client
                .put_object()
                .bucket(&state.bucket)
                .key(thumb_key)
                .body(ByteStream::from(thumb.clone()))
                .content_type("image/webp"),
        );
        thumb_request.send().await.map_err(s3_error)?;
    }
    if let Some(path) = output_to_clean {
        let _ = tokio::fs::remove_file(path).await;
    }

    Ok((
        StatusCode::CREATED,
        Json(StoredObject {
            key,
            original_name,
            mime_type,
            size_bytes,
            kind: kind.to_string(),
            owner_id,
            namespace,
            reference_id,
            created_at: now,
            thumbnail_key,
        }),
    ))
}

async fn list_objects(
    State(state): State<AppState>,
    Query(query): Query<ListObjectsQuery>,
) -> Result<Json<Vec<StoredObject>>, (StatusCode, String)> {
    let owner_id = safe_segment(&query.owner_id)?;
    let namespace = safe_segment(&query.namespace)?;
    let reference_id = safe_segment(&query.reference_id)?;
    let prefix = format!("{namespace}/{reference_id}/");
    let listed = state
        .client
        .list_objects_v2()
        .bucket(&state.bucket)
        .prefix(prefix)
        .send()
        .await
        .map_err(s3_error)?;
    let mut objects = Vec::new();
    for object in listed.contents() {
        let Some(key) = object.key() else {
            continue;
        };
        let head = state
            .client
            .head_object()
            .bucket(&state.bucket)
            .key(key)
            .send()
            .await
            .map_err(s3_error)?;
        let metadata = head.metadata().cloned().unwrap_or_default();
        if metadata
            .get("owner-id")
            .is_some_and(|value| value == &owner_id)
        {
            objects.push(stored_object_from_head(
                key.to_string(),
                &metadata,
                head.content_type(),
                head.content_length().unwrap_or(0),
            ));
        }
    }
    objects.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(Json(objects))
}

async fn head_object_checked(
    state: &AppState,
    key: &str,
) -> Result<aws_sdk_s3::operation::head_object::HeadObjectOutput, (StatusCode, String)> {
    match state
        .client
        .head_object()
        .bucket(&state.bucket)
        .key(key)
        .send()
        .await
    {
        Ok(head) => Ok(head),
        Err(error) if error.as_service_error().is_some_and(|e| e.is_not_found()) => Err((
            StatusCode::NOT_FOUND,
            "File tidak ditemukan".to_string(),
        )),
        Err(error) => Err(s3_error(error)),
    }
}

async fn object_metadata(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<StoredObject>, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let head = head_object_checked(&state, &key).await?;
    Ok(Json(stored_object_from_head(
        key,
        &head.metadata().cloned().unwrap_or_default(),
        head.content_type(),
        head.content_length().unwrap_or(0),
    )))
}

async fn download_object(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let head = head_object_checked(&state, &key).await?;
    let total = head.content_length().unwrap_or(0).max(0) as u64;

    match headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range(value, total))
    {
        Some((start, end)) if start < total => {
            let end = end.min(total.saturating_sub(1));
            if end < start {
                return full_object(&state, &key, total).await;
            }
            let object = state
                .client
                .get_object()
                .bucket(&state.bucket)
                .key(&key)
                .range(format!("bytes={start}-{end}"))
                .send()
                .await
                .map_err(s3_error)?;
            let mime = object
                .content_type()
                .unwrap_or("application/octet-stream")
                .to_string();
            let original_name = original_name_from(object.metadata(), &key);
            let response = Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, &mime)
                .header(
                    header::CONTENT_DISPOSITION,
                    download_disposition(&original_name, &mime),
                )
                .header(header::CONTENT_LENGTH, end - start + 1)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                )
                .header(header::ACCEPT_RANGES, "bytes")
                .body(Body::from_stream(ReaderStream::new(
                    object.body.into_async_read(),
                )))
                .map_err(bad_request)?;
            Ok(response)
        }
        _ => full_object(&state, &key, total).await,
    }
}

async fn full_object(
    state: &AppState,
    key: &str,
    total: u64,
) -> Result<Response, (StatusCode, String)> {
    let object = state
        .client
        .get_object()
        .bucket(&state.bucket)
        .key(key)
        .send()
        .await
        .map_err(s3_error)?;
    let mime = object
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();
    let original_name = original_name_from(object.metadata(), key);
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &mime)
        .header(
            header::CONTENT_DISPOSITION,
            download_disposition(&original_name, &mime),
        )
        .header(header::CONTENT_LENGTH, total)
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::from_stream(ReaderStream::new(
            object.body.into_async_read(),
        )))
        .map_err(bad_request)?;
    Ok(response)
}

async fn download_headers(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let head = head_object_checked(&state, &key).await?;
    let mime = head
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();
    let original_name = original_name_from(head.metadata(), &key);
    let total = head.content_length().unwrap_or(0).max(0) as u64;
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &mime)
        .header(
            header::CONTENT_DISPOSITION,
            download_disposition(&original_name, &mime),
        )
        .header(header::CONTENT_LENGTH, total)
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::empty())
        .map_err(bad_request)?;
    Ok(response)
}

fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?;
    if spec.is_empty() || !spec.contains('-') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end = if end.trim().is_empty() {
        total.saturating_sub(1)
    } else {
        end.trim().parse().ok()?
    };
    Some((start, end))
}

fn original_name_from(metadata: Option<&HashMap<String, String>>, key: &str) -> String {
    metadata
        .and_then(|meta| meta.get("original-name"))
        .cloned()
        .unwrap_or_else(|| key.rsplit('/').next().unwrap_or("download").to_string())
}

async fn delete_object(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(query): Query<DeleteObjectQuery>,
) -> Result<StatusCode, (StatusCode, String)> {
    let key = valid_key(&key)?;
    let head = head_object_checked(&state, &key).await?;
    if head
        .metadata()
        .and_then(|meta| meta.get("owner-id"))
        .is_none_or(|owner| owner != &query.owner_id)
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Hanya pemilik file yang dapat menghapusnya".to_string(),
        ));
    }
    let kind = head.metadata().and_then(|meta| meta.get("kind")).map(String::as_str);
    state
        .client
        .delete_object()
        .bucket(&state.bucket)
        .key(&key)
        .send()
        .await
        .map_err(s3_error)?;
    if kind == Some("image") {
        let thumb_key = format!("{}-thumb.webp", key.trim_end_matches(".webp"));
        state
            .client
            .delete_object()
            .bucket(&thumb_key)
            .send()
            .await
            .map_err(s3_error)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

struct ProcessedUpload {
    bytes: Vec<u8>,
    mime_type: String,
    extension: &'static str,
    kind: &'static str,
    size_bytes: usize,
    thumbnail: Option<Vec<u8>>,
}

fn fit_dimensions(width: u32, height: u32, max_dim: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= max_dim || longest == 0 {
        (width, height)
    } else {
        let scale = max_dim as f64 / longest as f64;
        (
            ((width as f64 * scale).round() as u32).max(1),
            ((height as f64 * scale).round() as u32).max(1),
        )
    }
}

fn encode_webp(image: &image::DynamicImage, quality: f32) -> Vec<u8> {
    if image.color().has_alpha() {
        let rgba = image.to_rgba8();
        Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
            .encode(quality)
            .to_vec()
    } else {
        let rgb = image.to_rgb8();
        Encoder::from_rgb(rgb.as_raw(), rgb.width(), rgb.height())
            .encode(quality)
            .to_vec()
    }
}

fn encode_jpeg(image: &image::DynamicImage, quality: u8) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut buffer, quality);
    encoder
        .encode(
            image.as_bytes(),
            image.width(),
            image.height(),
            image.color().into(),
        )
        .expect("JPEG encoding should not fail on decoded image");
    buffer
}

fn process_upload(
    limits: UploadLimits,
    bytes: &[u8],
    mime_type: &str,
    filename: &str,
) -> Result<ProcessedUpload, (StatusCode, String)> {
    if is_image(mime_type) {
        if bytes.len() > limits.max_image_bytes {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "Ukuran gambar maksimal {} MB",
                    limits.max_image_bytes / (1024 * 1024)
                ),
            ));
        }
        let image = image::load_from_memory(bytes).map_err(|_| {
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "File gambar tidak valid".to_string(),
            )
        })?;
        let (width, height) = image.dimensions();
        if width.saturating_mul(height) > limits.max_image_pixels {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "Resolusi gambar terlalu besar".to_string(),
            ));
        }

        let (main_w, main_h) = fit_dimensions(width, height, limits.image_max_dimension);
        let main_image = if main_w != width || main_h != height {
            image.resize(main_w, main_h, FilterType::Lanczos3)
        } else {
            image.clone()
        };
        let candidate = encode_webp(&main_image, 80.0);

        let (thumb_w, thumb_h) = fit_dimensions(width, height, limits.thumbnail_dimension);
        let thumb_image = if thumb_w != width || thumb_h != height {
            image.resize(thumb_w, thumb_h, FilterType::Lanczos3)
        } else {
            image.clone()
        };
        let thumbnail = encode_webp(&thumb_image, 80.0);

        if candidate.len() < bytes.len() {
            return Ok(ProcessedUpload {
                size_bytes: candidate.len(),
                bytes: candidate,
                mime_type: "image/webp".to_string(),
                extension: "webp",
                kind: "image",
                thumbnail: Some(thumbnail),
            });
        }
        let jpeg_bytes = encode_jpeg(&main_image, 85);
        if jpeg_bytes.len() < bytes.len() {
            return Ok(ProcessedUpload {
                size_bytes: jpeg_bytes.len(),
                bytes: jpeg_bytes,
                mime_type: "image/jpeg".to_string(),
                extension: "jpg",
                kind: "image",
                thumbnail: Some(thumbnail),
            });
        }
        return Ok(ProcessedUpload {
            size_bytes: bytes.len(),
            bytes: bytes.to_vec(),
            mime_type: mime_type.to_string(),
            extension: extension_for(filename, mime_type),
            kind: "image",
            thumbnail: Some(thumbnail),
        });
    }
    if !is_document(mime_type) {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Jenis file tidak didukung".to_string(),
        ));
    }
    if bytes.len() > limits.max_document_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "Ukuran dokumen maksimal {} MB",
                limits.max_document_bytes / (1024 * 1024)
            ),
        ));
    }
    Ok(ProcessedUpload {
        size_bytes: bytes.len(),
        bytes: bytes.to_vec(),
        mime_type: mime_type.to_string(),
        extension: extension_for(filename, mime_type),
        kind: "document",
        thumbnail: None,
    })
}

fn is_image(mime: &str) -> bool {
    matches!(mime, "image/jpeg" | "image/png" | "image/webp")
}
fn is_document(mime: &str) -> bool {
    matches!(
        mime,
        "application/pdf"
            | "application/msword"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.ms-excel"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "text/plain"
    )
}
fn is_video(mime: &str) -> bool {
    matches!(
        mime,
        "video/mp4"
            | "video/quicktime"
            | "video/webm"
            | "video/x-matroska"
            | "video/x-msvideo"
            | "video/3gpp"
            | "video/3gpp2"
            | "video/mpeg"
            | "video/ogg"
    )
}

fn temp_ext(mime: &str) -> &'static str {
    match mime {
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/x-msvideo" => "avi",
        "video/3gpp" | "video/3gpp2" => "3gp",
        "video/mpeg" => "mpeg",
        "video/ogg" => "ogv",
        _ => "mp4",
    }
}

struct TranscodedVideo {
    path: std::path::PathBuf,
    mime_type: String,
    extension: &'static str,
    kind: &'static str,
    size_bytes: u64,
}

async fn transcode_video(
    limits: UploadLimits,
    input: &std::path::Path,
    filename: &str,
) -> Result<TranscodedVideo, (StatusCode, String)> {
    let input_len = tokio::fs::metadata(input).await.map_err(bad_request)?.len();
    if input_len > limits.max_video_bytes as u64 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "Ukuran video maksimal {} MB",
                limits.max_video_bytes / (1024 * 1024)
            ),
        ));
    }
    let output = std::env::temp_dir().join(format!("eco-video-out-{}.mp4", Uuid::new_v4()));
    let result = match run_ffmpeg(input, &output, true).await {
        Ok(()) => Ok(()),
        Err((StatusCode::REQUEST_TIMEOUT, _)) => {
            Err((
                StatusCode::REQUEST_TIMEOUT,
                "Pemrosesan video memakan waktu terlalu lama. Coba video yang lebih pendek atau lebih kecil.".to_string(),
            ))
        }
        Err(_) => {
            run_ffmpeg(input, &output, false).await
        }
    };
    if let Err((status, message)) = result {
        let _ = tokio::fs::remove_file(&output).await;
        return Err((status, message));
    }
    let size_bytes = tokio::fs::metadata(&output)
        .await
        .map_err(bad_request)?
        .len();
    if size_bytes == 0 {
        let _ = tokio::fs::remove_file(&output).await;
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("Gagal memproses video: {}", file_name_hint(filename)),
        ));
    }
    Ok(TranscodedVideo {
        path: output,
        mime_type: "video/mp4".to_string(),
        extension: "mp4",
        kind: "video",
        size_bytes,
    })
}

fn file_name_hint(filename: &str) -> String {
    let truncated: String = filename.chars().take(40).collect();
    if truncated == filename {
        filename.to_string()
    } else {
        format!("{truncated}…")
    }
}

async fn run_ffmpeg(
    input: &std::path::Path,
    output: &std::path::Path,
    with_audio: bool,
) -> Result<(), (StatusCode, String)> {
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("fast")
        .arg("-crf")
        .arg("26")
        .arg("-maxrate")
        .arg("2M")
        .arg("-bufsize")
        .arg("4M")
        .arg("-vf")
        .arg("scale='min(1280,iw)':-2")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-r")
        .arg("30");
    if with_audio {
        command.arg("-c:a").arg("aac").arg("-b:a").arg("128k");
    } else {
        command.arg("-an");
    }
    command
        .arg("-movflags")
        .arg("+faststart")
        .arg("-f")
        .arg("mp4")
        .arg(output)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = command.spawn().map_err(bad_request)?;
    let status = tokio::time::timeout(std::time::Duration::from_secs(120), child.wait()).await;
    match status {
        Ok(Ok(exit)) if exit.success() => Ok(()),
        Ok(Ok(_)) => Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "ffmpeg gagal memproses video".to_string(),
        )),
        Ok(Err(error)) => Err(bad_request(error)),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err((
                StatusCode::REQUEST_TIMEOUT,
                "ffmpeg gagal memproses video".to_string(),
            ))
        }
    }
}

fn download_disposition(filename: &str, mime: &str) -> HeaderValue {
    let encoded = filename
        .as_bytes()
        .iter()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                format!("{}", *byte as char)
            }
            _ => format!("%{:02X}", byte),
        })
        .collect::<String>();
    let kind = if is_image(mime) || is_video(mime) {
        "inline"
    } else {
        "attachment"
    };
    HeaderValue::from_str(&format!("{kind}; filename*=UTF-8''{encoded}"))
        .unwrap_or_else(|_| HeaderValue::from_str(kind).unwrap())
}

fn extension_for(filename: &str, mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/plain" => "txt",
        mime if is_video(mime) => "mp4",
        _ if filename.ends_with(".bin") => "bin",
        _ => "bin",
    }
}
fn mime_from_name(name: &str) -> String {
    if name.ends_with(".png") {
        "image/png"
    } else if name.ends_with(".webp") {
        "image/webp"
    } else if name.ends_with(".pdf") {
        "application/pdf"
    } else if name.ends_with(".mp4")
        || name.ends_with(".mov")
        || name.ends_with(".m4v")
        || name.ends_with(".webm")
        || name.ends_with(".mkv")
        || name.ends_with(".avi")
        || name.ends_with(".3gp")
        || name.ends_with(".ogv")
    {
        "video/mp4"
    } else {
        "image/jpeg"
    }
    .to_string()
}
fn required_field<'a>(
    fields: &'a HashMap<String, String>,
    name: &str,
) -> Result<&'a str, (StatusCode, String)> {
    fields
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("Field {name} wajib diisi")))
}
fn safe_segment(value: &str) -> Result<String, (StatusCode, String)> {
    if !value.is_empty()
        && value.len() <= 120
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Ok(value.to_string())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "Identifier storage tidak valid".to_string(),
        ))
    }
}
fn valid_key(key: &str) -> Result<String, (StatusCode, String)> {
    if key.split('/').all(|segment| safe_segment(segment).is_ok()) {
        Ok(key.to_string())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "Key storage tidak valid".to_string(),
        ))
    }
}
fn stored_object_from_head(
    key: String,
    metadata: &HashMap<String, String>,
    content_type: Option<&str>,
    content_length: i64,
) -> StoredObject {
    let kind = metadata
        .get("kind")
        .cloned()
        .unwrap_or_else(|| "document".to_string());
    let key_trimmed_webp = key.trim_end_matches(".webp").to_string();
    StoredObject {
        key,
        original_name: metadata
            .get("original-name")
            .cloned()
            .unwrap_or_else(|| "file".to_string()),
        mime_type: content_type
            .unwrap_or("application/octet-stream")
            .to_string(),
        size_bytes: content_length.max(0) as u64,
        thumbnail_key: (kind == "image").then(|| {
            format!("{}-thumb.webp", key_trimmed_webp)
        }),
        kind,
        owner_id: metadata.get("owner-id").cloned().unwrap_or_default(),
        namespace: metadata.get("namespace").cloned().unwrap_or_default(),
        reference_id: metadata.get("reference-id").cloned().unwrap_or_default(),
        created_at: metadata
            .get("created-at")
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(Utc::now),
    }
}
fn bad_request<E: std::fmt::Display>(error: E) -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        format!("Upload tidak valid: {error}"),
    )
}
fn s3_error<E: std::fmt::Display>(error: E) -> (StatusCode, String) {
    tracing::error!(%error, "S3 operation failed");
    (
        StatusCode::BAD_GATEWAY,
        "Storage tidak tersedia".to_string(),
    )
}

pub async fn bootstrap() -> anyhow::Result<axum::Router> {
    let endpoint = required_env("S3_ENDPOINT");
    let bucket = env("S3_BUCKET", "eco-storage");
    let region = env("S3_REGION", "us-east-1");
    let limits = UploadLimits {
        max_image_bytes: (env_u64("MAX_IMAGE_MB", 10) * 1024 * 1024) as usize,
        max_document_bytes: (env_u64("MAX_DOCUMENT_MB", 50) * 1024 * 1024) as usize,
        max_image_pixels: env_u64("MAX_IMAGE_PIXELS", 40_000_000) as u32,
        max_video_bytes: (env_u64("MAX_VIDEO_MB", 200) * 1024 * 1024) as usize,
        image_max_dimension: env_u64("IMAGE_MAX_DIMENSION", 1600) as u32,
        thumbnail_dimension: env_u64("THUMBNAIL_DIMENSION", 400) as u32,
    };
    let credentials = Credentials::new(
        required_env("S3_ACCESS_KEY"),
        required_env("S3_SECRET_KEY"),
        None,
        None,
        "eco-managed-minio",
    );
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region))
        .credentials_provider(credentials)
        .endpoint_url(endpoint)
        .load()
        .await;
    let s3_config = aws_sdk_s3::config::Builder::from(&shared)
        .force_path_style(true)
        .build();
    let state = AppState {
        client: Client::from_conf(s3_config),
        bucket,
        limits,
    };
    ensure_bucket(&state).await?;
    Ok(build_router(state))
}
