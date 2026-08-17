//! The CGI header fold: one deterministic value per field name for the
//! `$_SERVER` `HTTP_*` mapping. The boundary carries one entry per field line;
//! only the CGI-serving modes fold, once, at request construction (ReqC).

/// The separator joining repeats of `name`, or `None` for a singleton field -
/// first line wins, the rest are dropped (joining a second `Authorization` into
/// the first would corrupt the credential php-src decodes). Combining is legal
/// only for comma-list fields (RFC 9110 §5.3,
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.3); `Cookie` rejoins on
/// `"; "` (RFC 6265 §4.2.1,
/// https://www.rfc-editor.org/rfc/rfc6265#section-4.2.1). A repeated `Host` is
/// the front's 400 (RFC 9112 §3.2); one that slipped through joins like any
/// list field and fails closed in whatever parses `HTTP_HOST`.
pub(crate) fn field_line_separator(name: &str) -> Option<&'static [u8]> {
    const SINGLETON: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "content-type",
        "content-length",
        "referer",
        "from",
    ];
    if SINGLETON.iter().any(|f| name.eq_ignore_ascii_case(f)) {
        None
    } else if name.eq_ignore_ascii_case("cookie") {
        Some(b"; ")
    } else {
        Some(b", ")
    }
}

/// Fold per-line header entries to one entry per name (case-insensitive, first-seen
/// name spelling and order kept): list fields join on their separator, singleton
/// fields keep the first line. The CGI mapping needs exactly one value per name -
/// `HTTP_*` registration is last-write-wins, so unfolded repeats would silently
/// keep whichever line landed last.
pub(crate) fn fold_field_lines(headers: &[(String, Vec<u8>)]) -> Vec<(String, Vec<u8>)> {
    let mut folded: Vec<(String, Vec<u8>)> = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        match folded
            .iter_mut()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
        {
            None => folded.push((name.clone(), value.clone())),
            Some((n, joined)) => {
                if let Some(sep) = field_line_separator(n) {
                    joined.extend_from_slice(sep);
                    joined.extend_from_slice(value);
                }
            }
        }
    }
    folded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn repeated_field_lines_fold_on_their_separator() {
        let folded = fold_field_lines(&hdrs(&[
            ("cookie", "a=1"),
            ("x-forwarded-for", "1.2.3.4"),
            ("Cookie", "b=2"),
            ("x-forwarded-for", "5.6.7.8"),
        ]));
        assert_eq!(
            folded,
            hdrs(&[
                ("cookie", "a=1; b=2"),
                ("x-forwarded-for", "1.2.3.4, 5.6.7.8"),
            ])
        );
    }

    #[test]
    fn repeated_singleton_field_lines_keep_only_the_first() {
        let folded = fold_field_lines(&hdrs(&[
            ("authorization", "Bearer one"),
            ("Authorization", "Bearer two"),
            ("content-type", "text/plain"),
        ]));
        assert_eq!(
            folded,
            hdrs(&[
                ("authorization", "Bearer one"),
                ("content-type", "text/plain")
            ])
        );
    }
}
