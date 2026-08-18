/// Separator joining repeats of `name`; `None` marks a singleton field, where the first line wins and the rest are dropped.
/// Combining is legal only for comma-list fields: https://www.rfc-editor.org/rfc/rfc9110#section-5.3
/// `Cookie` rejoins on `"; "`: https://www.rfc-editor.org/rfc/rfc6265#section-4.2.1
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

/// `HTTP_*` registration is last-write-wins, so repeats must be folded to one entry per name here.
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
