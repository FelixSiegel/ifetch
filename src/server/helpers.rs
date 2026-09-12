use std::io::Cursor;
use std::sync::{Mutex, MutexGuard};
use tiny_http::{Header, Response};

/// Acquires a lock on a mutex, recovering gracefully if another thread panicked while holding it.
pub fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Validates that a path or URL segment is non-empty and contains no directory traversal tokens.
pub fn is_valid_segment(s: &str) -> bool {
    !s.is_empty() && s != "." && !s.contains('/') && !s.contains('\\') && !s.contains("..")
}

/// Splits and validates a compound chapter ID into `(manga_id, chapter_number)`.
///
/// Compound format: `<manga_id>::<chapter_number>`.
pub fn parse_chapter_id(chap_id: &str) -> Option<(&str, &str)> {
    let (id, number) = chap_id.split_once("::")?;
    if is_valid_segment(id) && is_valid_segment(number) {
        Some((id, number))
    } else {
        None
    }
}

/// Builds a 200 OK JSON HTTP response from any serializable payload.
pub fn json_response<T: serde::Serialize>(data: &T) -> anyhow::Result<Response<Cursor<Vec<u8>>>> {
    let json = serde_json::to_string(data)?;
    Ok(Response::from_string(json)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()))
}

/// Builds a plain-text HTTP response with a custom status code.
pub fn text_response(status: u16, msg: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_string(msg).with_status_code(status)
}

/// Returns a 400 Bad Request HTTP response.
pub fn bad_request(msg: &str) -> Response<Cursor<Vec<u8>>> {
    text_response(400, msg)
}

/// Returns a 404 Not Found HTTP response.
pub fn not_found(msg: &str) -> Response<Cursor<Vec<u8>>> {
    text_response(404, msg)
}

/// Returns a 202 Accepted empty HTTP response.
pub fn accepted_empty() -> Response<Cursor<Vec<u8>>> {
    Response::from_string("")
        .with_status_code(202)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap())
}

/// Returns an image HTTP response with appropriate Content-Type and immutable caching headers.
pub fn image_response(data: &[u8], content_type: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(data.to_vec())
        .with_header(Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap())
        .with_header(
            Header::from_bytes(
                &b"Cache-Control"[..],
                &b"public, max-age=31536000, immutable"[..],
            )
            .unwrap(),
        )
}
