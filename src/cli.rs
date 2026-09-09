use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "sid",
    about = "Interactive sed with a live git-style diff preview",
    after_help = "Opens a TUI by default. With no FILE args, scans the current directory recursively (skips hidden paths and common build dirs). Type a sed expression to preview changes; Ctrl+S applies in-place."
)]
pub struct Args {
    /// Prefill the sed expression editor
    #[arg(short = 'e', long = "expression")]
    pub expression: Option<String>,

    /// Path to the sed binary (default: sed on PATH)
    #[arg(long = "sed-bin", default_value = "sed")]
    pub sed_bin: PathBuf,

    /// Backup suffix for in-place apply (GNU sed -i). Pass `-i` alone for no backup.
    #[arg(short = 'i', long = "in-place", num_args = 0..=1, default_missing_value = "")]
    pub in_place: Option<String>,

    /// Files to preview / edit (default: all non-hidden files under `.`, recursively)
    pub files: Vec<PathBuf>,
}
