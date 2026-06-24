use php_sys::Rapira;

fn main() -> anyhow::Result<()> {
    let r: Rapira = Rapira::boot(php_sys::Mode::Classic, 1)?;
    let _handler: php_sys::RapiraHandle = r.handle()?;

    Ok(())
}
