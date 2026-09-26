use std::{env, error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=../../packaging/icons/sylvops.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let icon =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("../../packaging/icons/sylvops.ico");
    winresource::WindowsResource::new()
        .set_icon(icon.to_str().ok_or("SylvOps icon path is not UTF-8")?)
        .compile()?;
    Ok(())
}
