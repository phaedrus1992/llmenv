use clap::Parser;

#[derive(Parser)]
#[command(name = "llme", version)]
struct Cli {}

pub fn run() -> anyhow::Result<()> {
    let _ = Cli::parse();
    Ok(())
}
