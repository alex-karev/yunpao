use anyhow::Result;
mod cli;
use colored::Colorize;
use cliclack::outro_cancel;

fn main() -> Result<()> {
    if let Err(err) = cli::run() {
        let mut buff = format!("{}: {err}", "Error".bold().red());
        err.chain().skip(1).for_each(|cause| {
            buff = format!("{buff}\n  -> {cause}");
        });
        outro_cancel(buff)?;
        std::process::exit(1);
    }
    Ok(())
}
