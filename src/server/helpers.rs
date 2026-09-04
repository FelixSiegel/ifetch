use std::io::Cursor;
use std::sync::{Mutex, MutexGuard};
use tiny_http::{Header, Response};

pub fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn is_valid_segment(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains('\\') && !s.contains("..")
}

pub fn parse_chapter_id(chap_id: &str) -> Option<(&str, &str)> {
    let (id, number) = chap_id.split_once("::")?;
    if is_valid_segment(id) && is_valid_segment(number) {
        Some((id, number))
    } else {
        None
    }
}

pub fn json_response<T: serde::Serialize>(data: &T) -> anyhow::Result<Response<Cursor<Vec<u8>>>> {
    let json = serde_json::to_string(data)?;
    Ok(Response::from_string(json)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()))
}

pub fn text_response(status: u16, msg: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_string(msg).with_status_code(status)
}

pub fn bad_request(msg: &str) -> Response<Cursor<Vec<u8>>> {
    text_response(400, msg)
}

pub fn not_found(msg: &str) -> Response<Cursor<Vec<u8>>> {
    text_response(404, msg)
}

pub fn accepted_empty() -> Response<Cursor<Vec<u8>>> {
    Response::from_string("")
        .with_status_code(202)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/plain"[..]).unwrap())
}

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
