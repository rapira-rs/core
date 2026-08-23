use extension_api::{Peer, Request};

use crate::Config;

pub(crate) fn build(
    parts: &http::request::Parts,
    authority: Option<Vec<u8>>,
    body: Vec<u8>,
    peer: &Peer,
    cfg: &Config,
) -> Request {
    let protocol = match parts.version {
        http::Version::HTTP_11 => "HTTP/1.1".to_owned(),
        http::Version::HTTP_10 => "HTTP/1.0".to_owned(),
        http::Version::HTTP_2 => "HTTP/2.0".to_owned(),
        http::Version::HTTP_3 => "HTTP/3.0".to_owned(),
        v => format!("{v:?}"),
    };
    Request {
        method: parts.method.as_str().to_owned(),
        uri: parts.uri.to_string(),
        target: None,
        authority,
        https: peer.https,
        protocol,
        remote: peer.remote.clone(),
        server: peer.server.clone(),
        server_name: cfg.server_name.clone(),
        server_port: cfg.server_port,
        tls: None,
        received_at: Some(peer.received_at),
        headers: parts
            .headers
            .iter()
            .map(|(n, v)| (n.as_str().to_owned(), v.as_bytes().to_vec()))
            .collect(),
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_api::Addr;

    fn peer() -> Peer {
        Peer {
            remote: Addr::Inet(([127, 0, 0, 1], 40000).into()),
            server: Addr::Inet(([127, 0, 0, 1], 8000).into()),
            https: false,
            received_at: 1.5,
        }
    }

    /// One FieldLines entry per field line, values in per-name wire order, names lowercase.
    #[test]
    fn headers_arrive_per_line_in_per_name_order() {
        let req = http::Request::builder()
            .method("GET")
            .uri("/a/b?x=1")
            .header("X-Probe", "one")
            .header("Accept", "text/*")
            .header("x-probe", "two")
            .body(())
            .unwrap();
        let (parts, ()) = req.into_parts();
        let built = build(
            &parts,
            Some(b"e2e".to_vec()),
            Vec::new(),
            &peer(),
            &Config::default(),
        );
        let probes: Vec<_> = built
            .headers
            .iter()
            .filter(|(n, _)| n == "x-probe")
            .map(|(_, v)| v.as_slice())
            .collect();
        assert_eq!(probes, [b"one".as_slice(), b"two".as_slice()]);
        assert_eq!(built.uri, "/a/b?x=1");
        assert_eq!(built.protocol, "HTTP/1.1");
        assert_eq!(built.authority.as_deref(), Some(&b"e2e"[..]));
        assert_eq!(built.received_at, Some(1.5));
    }
}
