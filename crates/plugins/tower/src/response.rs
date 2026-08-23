use extension_api::{BoxError, FieldLines, HttpResponse};
use http::header::{CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, HeaderMap, HeaderName, HeaderValue};
use http_body_util::BodyExt;

pub(crate) fn empty_body() -> extension_api::Body {
    http_body_util::Empty::<bytes::Bytes>::new()
        .map_err(BoxError::from)
        .boxed_unsync()
}

pub(crate) fn error_response(status: http::StatusCode) -> HttpResponse {
    let mut res = http::Response::new(empty_body());
    *res.status_mut() = status;
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    res.headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    res
}

pub(crate) fn skip_response_header(name: &str) -> bool {
    [
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "upgrade",
        "trailer",
        "te",
        "proxy-connection",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

pub(crate) fn connection_named_headers(value: &[u8], out: &mut Vec<String>) {
    for tok in value.split(|&b| b == b',') {
        let tok = String::from_utf8_lossy(tok).trim().to_ascii_lowercase();
        if !tok.is_empty() {
            out.push(tok);
        }
    }
}

pub(crate) fn response_headers(
    headers: FieldLines,
    content_length: Option<u64>,
    no_body: bool,
) -> HeaderMap {
    let mut map = HeaderMap::with_capacity(headers.len() + 1);
    let mut conn_named: Vec<String> = Vec::new();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("connection") {
            connection_named_headers(&value, &mut conn_named);
            continue;
        }
        if skip_response_header(&name) {
            continue;
        }
        match (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_bytes(&value),
        ) {
            (Ok(n), Ok(v)) => {
                map.append(n, v);
            }
            _ => {
                tracing::debug!(target: "http", "dropped response header {name}: unrepresentable name or value")
            }
        }
    }
    for tok in &conn_named {
        if let Ok(name) = HeaderName::try_from(tok.as_str()) {
            map.remove(name);
        }
    }
    if !no_body && let Some(n) = content_length {
        map.insert(CONTENT_LENGTH, HeaderValue::from(n));
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> FieldLines {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn connection_value_cannot_strip_framing() {
        let map = response_headers(
            hdrs(&[
                ("Connection", "content-length, x-drop"),
                ("X-Drop", "1"),
                ("X-Keep", "2"),
            ]),
            Some(7),
            false,
        );
        assert_eq!(map.get("content-length").unwrap().as_bytes(), b"7");
        assert!(map.get("x-drop").is_none());
        assert_eq!(map.get("x-keep").unwrap().as_bytes(), b"2");
        assert!(map.get("connection").is_none());
    }

    #[test]
    fn bodyless_statuses_get_no_content_length() {
        let map = response_headers(hdrs(&[("X-A", "1")]), Some(0), true);
        assert!(map.get("content-length").is_none());
        assert!(map.get("transfer-encoding").is_none());
        assert_eq!(map.get("x-a").unwrap().as_bytes(), b"1");
    }

    /// A space is not a tchar, so no front can put this name on the wire; dropping the field must not cost the rest of the response.
    #[test]
    fn unrepresentable_header_is_dropped_not_fatal() {
        let map = response_headers(
            hdrs(&[("Content Type", "text/html"), ("X-Keep", "2")]),
            Some(3),
            false,
        );
        assert!(map.get("content type").is_none());
        assert_eq!(map.get("x-keep").unwrap().as_bytes(), b"2");
        assert_eq!(map.get("content-length").unwrap().as_bytes(), b"3");
    }

    /// An unrepresentable value (control byte) drops the field the same way an unrepresentable name does.
    #[test]
    fn unrepresentable_value_is_dropped_not_fatal() {
        let map = response_headers(
            vec![
                ("X-Ctl".to_owned(), b"\x01".to_vec()),
                ("X-Keep".to_owned(), b"ok".to_vec()),
            ],
            None,
            false,
        );
        assert!(map.get("x-ctl").is_none());
        assert_eq!(map.get("x-keep").unwrap().as_bytes(), b"ok");
    }

    #[test]
    fn php_framing_headers_never_reach_the_wire() {
        let map = response_headers(
            hdrs(&[("Content-Length", "999"), ("Transfer-Encoding", "chunked")]),
            Some(4),
            false,
        );
        assert_eq!(map.get("content-length").unwrap().as_bytes(), b"4");
        assert!(map.get("transfer-encoding").is_none());
    }

    #[test]
    fn connection_tokens_are_split_trimmed_and_lowercased() {
        let mut out = Vec::new();
        connection_named_headers(b"  Keep-Alive , ,X-Foo\t", &mut out);
        assert_eq!(out, vec!["keep-alive".to_owned(), "x-foo".to_owned()]);
    }

    #[test]
    fn hop_by_hop_names_match_case_insensitively() {
        assert!(skip_response_header("Transfer-Encoding"));
        assert!(skip_response_header("PROXY-CONNECTION"));
        assert!(!skip_response_header("content-type"));
    }

    #[test]
    fn error_response_is_minimal_and_closes() {
        let res = error_response(http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(res.status(), 413);
        assert_eq!(res.headers().get("connection").unwrap(), "close");
        assert_eq!(
            res.headers().get("cache-control").unwrap(),
            "private, no-store"
        );
    }
}
