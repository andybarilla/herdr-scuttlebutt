mod cli;
mod daemon;
mod git_org;
mod groups;
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
        /// Target this group instead of resolving from the calling cwd
        #[arg(long)]
        group: Option<String>,
    },
    /// Print room messages
    Read {
        /// Only messages with id greater than this
        #[arg(long)]
        since: Option<u64>,
        /// Max messages when --since is not given
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Target this group instead of resolving from the calling cwd
        #[arg(long)]
        group: Option<String>,
    },
    /// List room members
    Agents {
        /// Target this group instead of resolving from the calling cwd
        #[arg(long)]
        group: Option<String>,
    },
    /// List groups and their members
    Groups,
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
    /// Print the focused workspace's checkout path (used by the pane scripts)
    #[command(hide = true)]
    SessionCwd,
    /// Open the chat TUI
    Tui {
        /// Target this group instead of resolving from the calling cwd
        #[arg(long)]
        group: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let herd = herd::RealHerd;
    match args.cmd {
        Cmd::Post {
            text,
            as_name,
            group,
        } => cli::cmd_post(group.as_deref(), &herd, as_name.as_deref(), &text),
        Cmd::Read {
            since,
            limit,
            group,
        } => cli::cmd_read(group.as_deref(), since, limit),
        Cmd::Agents { group } => cli::cmd_agents(group.as_deref(), &herd),
        Cmd::Groups => cli::cmd_groups(&herd),
        Cmd::Daemon { agents } => {
            let pattern = agents
                .or_else(|| std::env::var("SCUTTLEBUTT_AGENTS").ok())
                .unwrap_or_default();
            daemon::run(
                &paths::session_dir()?,
                &daemon::AgentFilter::parse(&pattern),
            )
        }
        Cmd::DaemonStatus => {
            daemon::status(&paths::session_dir()?);
            Ok(())
        }
        Cmd::DaemonStop => daemon::stop(&paths::session_dir()?),
        Cmd::SessionCwd => {
            println!("{}", herd::focused_cwd()?);
            Ok(())
        }
        Cmd::Tui { group } => tui::run(group.as_deref()),
    }
}
