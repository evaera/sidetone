use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "sidetone",
    version,
    about = "Tray-only microphone monitoring",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Write diagnostic logs (maximum 1 MiB)
    #[arg(long)]
    pub log: bool,

    /// Run a twelve-second muted audio and delay-preset check
    #[arg(long)]
    pub smoke_test: bool,

    /// Run a twenty-five-second muted device and dropout recovery check
    #[arg(long)]
    pub reconnect_test: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run Sidetone at login (--remove to undo)
    Startup {
        /// Remove the startup entry instead of adding it
        #[arg(long)]
        remove: bool,
    },
}
