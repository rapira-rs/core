use extension_api::{Extension, Request, Response, Result, exec, register_extension};

struct Hello;

impl Extension for Hello {
    fn new() -> Self {
        Hello
    }

    async fn run(&mut self) -> Result<()> {
        // Two requests driven concurrently: `join!` starts both `exec` subtasks
        // before awaiting either, so they run in parallel through the PHP pool.
        let (a, b) = futures::join!(
            exec(Request::get("/?from=a")),
            exec(Request::get("/?from=b")),
        );

        // Each response must be its own — distinct bodies prove both ran.
        check(&a?, "ok:a")?;
        check(&b?, "ok:b")?;
        Ok(())
    }
}

fn check(res: &Response, expected_body: &str) -> Result<()> {
    if res.status != 200 {
        return Err(format!("expected 200, got {}", res.status));
    }
    if res.body != expected_body.as_bytes() {
        return Err(format!(
            "expected body {expected_body:?}, got {:?}",
            String::from_utf8_lossy(&res.body)
        ));
    }
    Ok(())
}

register_extension!(Hello);
