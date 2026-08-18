mod cli;
mod daemon;
mod herd;
mod log_store;
mod paths;
mod state;
mod tui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "scuttlebutt", about = "Chat room for herdr agents")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Post a message to the room
    Post {
        text: String,
        /// Post as this name instead of resolving from the calling pane
        #[arg(long = "as")]
        as_name: Option<String>,
    },
    /// Print room messages
    Read {
        /// Only messages with id greater than this
        #[arg(long)]
        since: Option<u64>,
        /// Max messages when --since is not given
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// List room members
    Agents,
    /// Run the delivery daemon in the foreground
    Daemon {
        /// Only enroll agents matching this comma-separated glob list
        /// (e.g. `gossip-*,reviewer`). Falls back to $SCUTTLEBUTT_AGENTS.
        /// With neither set, every named agent is enrolled.
        #[arg(long)]
        agents: Option<String>,
    },
    /// Show daemon status
    DaemonStatus,
    /// Stop the daemon
    DaemonStop,
    /// Open the chat TUI
    Tui,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let herd = herd::RealHerd;
    match args.cmd {
        Cmd::Post { text, as_name } => cli::cmd_post(&herd, as_name.as_deref(), &text),
        Cmd::Read { since, limit } => cli::cmd_read(since, limit),
        Cmd::Agents => cli::cmd_agents(&herd),
        Cmd::Daemon { agents } => {
            let pattern = agents
                .or_else(|| std::env::var("SCUTTLEBUTT_AGENTS").ok())
                .unwrap_or_default();
            daemon::run(&paths::room_dir()?, &daemon::AgentFilter::parse(&pattern))
        }
        Cmd::DaemonStatus => {
            daemon::status(&paths::room_dir()?);
            Ok(())
        }
        Cmd::DaemonStop => daemon::stop(&paths::room_dir()?),
        Cmd::Tui => tui::run(),
    }
}
