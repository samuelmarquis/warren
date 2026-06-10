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

mod names;
mod paths;
mod sessions;

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
        "new" | "create" => todo_role("new"),
        "ls" | "list" => todo_role("ls"),
        "kill" | "rm" => todo_role("kill"),
        "attach" => todo_role("attach"),
        "sessions" => sessions::cmd_sessions(),
        "hook" => todo_role("hook"),
        "__daemon" => todo_role("__daemon"),
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        _ => {
            eprintln!("warren: unknown command '{sub}' (try: warren help)");
            std::process::exit(1);
        }
    };
    let _ = rest;

    if let Err(e) = result {
        eprintln!("warren: {e:#}");
        std::process::exit(1);
    }
}

fn todo_role(name: &str) -> Result<()> {
    anyhow::bail!("'{name}' is not implemented yet in warren v1")
}
