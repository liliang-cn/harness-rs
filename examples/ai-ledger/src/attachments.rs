//! Chat attachment upload + serve. v1 supports images (jpeg/png/webp/heic/gif)
//! and PDF. Files are written under HARNESS_LEDGER_UPLOADS (default
//! ./uploads), one per-user dir. The DB just stores the path + metadata.
//!
//! The upload route is wired with a 20 MB body-limit override in
//! `server::serve` — the rest of /api keeps axum's 2 MB default.

use crate::auth::AuthCtx;
use crate::server::{ApiError, AppState};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, State},
    http::header,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

const MAX_BYTES: usize = 20 * 1024 * 1024; // 20 MB

fn upload_root() -> PathBuf {
    std::env::var("HARNESS_LEDGER_UPLOADS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./uploads"))
}

fn kind_for(mime: &str) -> Option<&'static str> {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/png" | "image/webp" | "image/heic" | "image/heif"
        | "image/gif" => Some("image"),
        "application/pdf" => Some("pdf"),
        _ => None,
    }
}

fn ext_for(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/heic" | "image/heif" => "heic",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

pub async fn upload_handler(
    State(_s): State<AppState>,
    auth: AuthCtx,
    mut multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut mime = String::new();
    let mut original_name: Option<String> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("multipart: {e}")))?
    {
        if field.name() == Some("file") {
            mime = field
                .content_type()
                .unwrap_or("application/octet-stream")
                .to_string();
            original_name = field.file_name().map(str::to_string);
            bytes = field
                .bytes()
                .await
                .map_err(|e| ApiError::BadRequest(format!("read: {e}")))?
                .to_vec();
            if bytes.len() > MAX_BYTES {
                return Err(ApiError::BadRequest(format!(
                    "file too large (>{} MB)",
                    MAX_BYTES / 1024 / 1024
                )));
            }
        }
    }
    if bytes.is_empty() {
        return Err(ApiError::BadRequest(
            "no file field in multipart body".into(),
        ));
    }
    let Some(kind) = kind_for(&mime) else {
        return Err(ApiError::BadRequest(format!(
            "unsupported mime type: {mime}"
        )));
    };

    let id_full = Uuid::new_v4().to_string().replace('-', "");
    let id: &str = &id_full[..16];
    let ext = ext_for(&mime);

    let user_dir = upload_root().join(&auth.user.id);
    std::fs::create_dir_all(&user_dir)
        .map_err(|e| ApiError::Internal(format!("mkdir: {e}")))?;
    let rel_path = format!("{}/{}.{ext}", auth.user.id, id);
    let full = upload_root().join(&rel_path);
    std::fs::write(&full, &bytes).map_err(|e| ApiError::Internal(format!("write: {e}")))?;

    let db = crate::server::open_db()?;
    db.insert_attachment(
        id,
        &auth.user.id,
        &mime,
        bytes.len() as i64,
        original_name.as_deref(),
        &rel_path,
        kind,
    )
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(json!({
        "id": id,
        "mime_type": mime,
        "size_bytes": bytes.len(),
        "kind": kind,
    })))
}

pub async fn serve_handler(
    State(_s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let db = crate::server::open_db()?;
    let rec = db
        .get_attachment(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("attachment not found".into()))?;
    let full = upload_root().join(&rec.path);
    let bytes =
        std::fs::read(&full).map_err(|e| ApiError::Internal(format!("read: {e}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, rec.mime_type.clone()),
            (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
        ],
        Body::from(bytes),
    )
        .into_response())
}
