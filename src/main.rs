use php_sys::Rapira;

fn main() -> anyhow::Result<()> {
    // execute script in classic mode, with 1 thread
    // this is the only for the configuration, that should come from PHP.
    // script for config can be passed via CLI options, or config
    let r: Rapira = Rapira::boot(php_sys::Mode::Classic, 1)?;
    let _handler: php_sys::RapiraHandle = r.handle()?;

    Ok(())
}
