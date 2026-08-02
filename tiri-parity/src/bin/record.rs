//! Record what sway does with a script.
//!
//! ```text
//! cargo run -p tiri-parity --bin record -- tiri-parity/fixtures/split.parity
//! ```
//!
//! Needs sway and a Wayland client on the machine; CI never runs this. The recording is
//! checked in, and re-running it after a sway upgrade produces a reviewable diff that says
//! what changed in sway.
//!
//! The file it reads and the file it writes are the same one: every `$ ` line is a command,
//! so a fixture is its own script. To add a scenario, write the commands and run the
//! recorder over it — the models underneath are filled in.

use std::path::{Path, PathBuf};

use tiri_parity::fixture::{Fixture, Step};
use tiri_parity::session::{self, Sway};

fn main() {
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if paths.is_empty() {
        eprintln!("usage: record <fixture.parity>...");
        std::process::exit(2);
    }

    // One session for all of them: starting sway is the slow part.
    let mut sway = match Sway::start() {
        Ok(sway) => sway,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    };
    let source = session::version().unwrap_or_else(|_| "sway".to_owned());

    let mut failed = false;
    for path in &paths {
        match record(&mut sway, &source, path) {
            Ok(()) => println!("recorded {}", path.display()),
            Err(err) => {
                eprintln!("{}: {err}", path.display());
                failed = true;
                break;
            }
        }
    }

    sway.stop();
    if failed {
        std::process::exit(1);
    }
}

fn record(sway: &mut Sway, source: &str, path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("cannot read: {err}"))?;
    let script = match Fixture::parse(&text) {
        Ok(fixture) => fixture.script(),
        // A fixture being written for the first time holds commands and nothing else.
        Err(_) => text.clone(),
    };

    sway.reset()?;
    let steps = sway
        .run(&parse_script(&script)?)?
        .into_iter()
        .map(|(command, model)| Step { command, model })
        .collect();

    let fixture = Fixture {
        source: source.to_owned(),
        steps,
    };
    std::fs::write(path, fixture.render()).map_err(|err| format!("cannot write: {err}"))
}

/// Commands, with blank lines and comments dropped.
///
/// Deliberately not validated against the replayer's table: the recorder passes commands to
/// sway verbatim, and a command sway understands but tiri has no `Op` for is a finding, not
/// a parse error.
fn parse_script(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.strip_prefix("$ ").unwrap_or(line);
        let line = line.split('#').next().unwrap_or("").trim();
        if !line.is_empty() {
            out.push(line.to_owned());
        }
    }
    if out.is_empty() {
        return Err("the script has no commands".into());
    }
    Ok(out)
}
