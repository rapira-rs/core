use integration_tests::{drain, fixture, php_lock, req};
use php_sys::{Mode, Rapira};

// One resident worker per extension; `?boom=1` switches its handler to the throwing
// call. 1 thread => the follow-up request rides the same interpreter, proving an
// uncaught extension throw leaves the worker serving.
fn run(name: &str, uris: &[&str]) -> anyhow::Result<Vec<(u16, String)>> {
    let _guard = php_lock();
    let r = Rapira::start(Mode::Worker(fixture(name)))?;
    let h = r.handle()?;
    let mut out = Vec::with_capacity(uris.len());
    for uri in uris {
        out.push(drain(h.handle_blocking(req(uri, name))?));
    }
    drop(h);
    r.shutdown();
    Ok(out)
}

fn success(name: &str, token: &str) -> anyhow::Result<()> {
    let out = run(name, &["/"])?;
    // fixtures echo "skip" when their extension is missing from this libphp build
    if out[0].1 == "skip" {
        return Ok(());
    }
    assert_eq!(out[0].0, 200, "{name} must serve 200 (got: {:?})", out[0]);
    assert!(
        out[0].1.contains(token),
        "{name} must echo {token:?} (got: {:?})",
        out[0].1
    );
    Ok(())
}

fn exception(name: &str, token: &str) -> anyhow::Result<()> {
    let out = run(name, &["/?boom=1", "/"])?;
    if out[0].1 == "skip" {
        return Ok(());
    }
    assert_eq!(
        out[0].0, 500,
        "{name} uncaught throw must be a 500 (got: {:?})",
        out[0]
    );
    assert_eq!(
        out[1].0, 200,
        "{name} must keep serving after the throw (got: {:?})",
        out[1]
    );
    assert!(
        out[1].1.contains(token),
        "{name} follow-up must echo {token:?} (got: {:?})",
        out[1].1
    );
    Ok(())
}

#[test]
fn zlib_success() -> anyhow::Result<()> {
    success("php_ext/zlib-worker.php", "zlib:rapira zlib")
}
#[test]
fn zlib_exception() -> anyhow::Result<()> {
    exception("php_ext/zlib-worker.php", "zlib:rapira zlib")
}

#[test]
fn curl_success() -> anyhow::Result<()> {
    success("php_ext/curl-worker.php", "curl:")
}
#[test]
fn curl_exception() -> anyhow::Result<()> {
    exception("php_ext/curl-worker.php", "curl:")
}

#[test]
fn ctype_success() -> anyhow::Result<()> {
    success("php_ext/ctype-worker.php", "ctype:1")
}
#[test]
fn ctype_exception() -> anyhow::Result<()> {
    exception("php_ext/ctype-worker.php", "ctype:1")
}

#[test]
fn mbstring_success() -> anyhow::Result<()> {
    success("php_ext/mbstring-worker.php", "mb:HÉLLO")
}
#[test]
fn mbstring_exception() -> anyhow::Result<()> {
    exception("php_ext/mbstring-worker.php", "mb:HÉLLO")
}

#[test]
fn iconv_success() -> anyhow::Result<()> {
    success("php_ext/iconv-worker.php", "iconv:iconv ok")
}
#[test]
fn iconv_exception() -> anyhow::Result<()> {
    exception("php_ext/iconv-worker.php", "iconv:iconv ok")
}

#[test]
fn openssl_success() -> anyhow::Result<()> {
    success("php_ext/openssl-worker.php", "openssl:64")
}
#[test]
fn openssl_exception() -> anyhow::Result<()> {
    exception("php_ext/openssl-worker.php", "openssl:64")
}

#[test]
fn fileinfo_success() -> anyhow::Result<()> {
    success("php_ext/fileinfo-worker.php", "finfo:text/plain")
}
#[test]
fn fileinfo_exception() -> anyhow::Result<()> {
    exception("php_ext/fileinfo-worker.php", "finfo:text/plain")
}

#[test]
fn tokenizer_success() -> anyhow::Result<()> {
    success("php_ext/tokenizer-worker.php", "tok:")
}
#[test]
fn tokenizer_exception() -> anyhow::Result<()> {
    exception("php_ext/tokenizer-worker.php", "tok:")
}

#[test]
fn phar_success() -> anyhow::Result<()> {
    success("php_ext/phar-worker.php", "phar:")
}
#[test]
fn phar_exception() -> anyhow::Result<()> {
    exception("php_ext/phar-worker.php", "phar:")
}

#[test]
fn dom_success() -> anyhow::Result<()> {
    success("php_ext/dom-worker.php", "dom:ok")
}
#[test]
fn dom_exception() -> anyhow::Result<()> {
    exception("php_ext/dom-worker.php", "dom:ok")
}

#[test]
fn simplexml_success() -> anyhow::Result<()> {
    success("php_ext/simplexml-worker.php", "sxml:ok")
}
#[test]
fn simplexml_exception() -> anyhow::Result<()> {
    exception("php_ext/simplexml-worker.php", "sxml:ok")
}

#[test]
fn xml_success() -> anyhow::Result<()> {
    success("php_ext/xml-worker.php", "xml:1")
}
#[test]
fn xml_exception() -> anyhow::Result<()> {
    exception("php_ext/xml-worker.php", "xml:1")
}

#[test]
fn xmlreader_success() -> anyhow::Result<()> {
    success("php_ext/xmlreader-worker.php", "xr:a")
}
#[test]
fn xmlreader_exception() -> anyhow::Result<()> {
    exception("php_ext/xmlreader-worker.php", "xr:a")
}

#[test]
fn xmlwriter_success() -> anyhow::Result<()> {
    success("php_ext/xmlwriter-worker.php", "xw:<v>ok</v>")
}
#[test]
fn xmlwriter_exception() -> anyhow::Result<()> {
    exception("php_ext/xmlwriter-worker.php", "xw:<v>ok</v>")
}

#[test]
fn pdo_sqlite_success() -> anyhow::Result<()> {
    success("php_ext/pdo_sqlite-worker.php", "pdo:ok")
}
#[test]
fn pdo_sqlite_exception() -> anyhow::Result<()> {
    exception("php_ext/pdo_sqlite-worker.php", "pdo:ok")
}

#[test]
fn sqlite3_success() -> anyhow::Result<()> {
    success("php_ext/sqlite3-worker.php", "sqlite:42")
}
#[test]
fn sqlite3_exception() -> anyhow::Result<()> {
    exception("php_ext/sqlite3-worker.php", "sqlite:42")
}

#[test]
fn filter_success() -> anyhow::Result<()> {
    success("php_ext/filter-worker.php", "filter:a@b.com")
}
#[test]
fn filter_exception() -> anyhow::Result<()> {
    exception("php_ext/filter-worker.php", "filter:a@b.com")
}

// No `exception` counterpart: this guards that OPcache actually started under our SAPI
// name, which PHP <= 8.4 gates on an allowlist (see build_sapi_module). Nothing to throw.
#[test]
fn opcache_success() -> anyhow::Result<()> {
    success("php_ext/opcache-worker.php", "opcache:enabled")
}
