//! Build-time packaging helper; this is not a runtime kernel selector.

use std::path::Path;

use anyhow::{bail, Context, Result};
use ww::packaging::{read_payload, stage_fhs, verify_fhs};

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 4 {
        bail!("usage: stage_fhs <stage|verify> KERNEL STATUS FHS_DIRECTORY");
    }
    let kernel = read_payload(Path::new(&arguments[1]))?;
    let status = read_payload(Path::new(&arguments[2]))?;
    let root = Path::new(&arguments[3]);
    match arguments[0].to_str().context("invalid operation")? {
        "stage" => stage_fhs(root, &kernel, &status),
        "verify" => verify_fhs(root, &kernel, &status),
        _ => bail!("operation must be stage or verify"),
    }
}
