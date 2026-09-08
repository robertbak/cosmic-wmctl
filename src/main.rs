use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    cosmic_wmctl::cli::Cli::parse().run()
}
