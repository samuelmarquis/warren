//! warren - a Claude Code agent multiplexer.
//!
//! One binary, several roles:
//!   warren                 open the dashboard (stateless viewer; reconstructs from daemons)
//!   warren new NAME [DIR] [COLOR 0-255] [new|resume|continue] [session-id]
//!   warren ls              list agents and their status
//!   warren kill NAME       terminate an agent
//!   warren attach NAME     raw single-agent viewer
//!   warren sessions        all resumable Claude sessions (id<TAB>mtime<TAB>cwd<TAB>title)
//!   warren hook STATE      called by Claude Code hooks; reports state to the agent daemon
//!   warren __daemon …      internal: the per-agent daemon process

mod cli;
mod daemon;
mod hooks;
mod names;
mod paths;
mod proto;
mod sessions;
mod spans;

use anyhow::Result;

const HELP: &str = "\
warren - a Claude Code agent multiplexer

  warren                 open the dashboard, or rebuild the view if agents are running
  warren new NAME [DIR] [COLOR 0-255] [new|resume|continue] [session-id]
  warren ls              list agents and their status
  warren kill NAME       terminate an agent
  warren attach NAME     view a single agent raw (no sidebar)
  warren sessions        list resumable Claude sessions
  warren help            show this help

Agents run as independent daemons: the dashboard (and your SSH connection)
can die and restart freely without touching them.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sub = args.first().map(String::as_str).unwrap_or("up");
    let rest = if args.is_empty() { &args[..] } else { &args[1..] };

    let result: Result<()> = match sub {
        "up" => todo_role("dashboard"),
        "new" | "create" => cli::cmd_new(rest),
        "ls" | "list" => cli::cmd_ls(),
        "kill" | "rm" => cli::cmd_kill(rest),
        "attach" => cli::cmd_attach(rest),
        "sessions" => sessions::cmd_sessions(),
        "hook" => hooks::cmd_hook(rest),
        "__daemon" => daemon::DaemonArgs::parse(rest).and_then(daemon::run),
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        _ => {
            eprintln!("warren: unknown command '{sub}' (try: warren help)");
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("warren: {e:#}");
        std::process::exit(1);
    }
}

fn todo_role(name: &str) -> Result<()> {
    anyhow::bail!("'{name}' is not implemented yet in warren v1")
}
