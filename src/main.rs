mod app;
mod cli;
mod diff_view;
mod escape;
mod files;
mod ui;
mod worker;

use anyhow::Result;
use clap::Parser;

use crate::app::App;
use crate::cli::Args;

fn main() -> Result<()> {
    let args = Args::parse();
    App::new(args)?.run()
}
