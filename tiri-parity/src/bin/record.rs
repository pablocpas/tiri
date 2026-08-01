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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tiri_parity::fixture::{Fixture, Step};
use tiri_parity::sway;

/// The output every recording runs on. Must match what the tiri replayer pins, or geometry
/// differences would only be reporting a difference in configuration.
const OUTPUT: (u32, u32) = (1280, 720);

/// How long to wait for sway, or for a window to appear or vanish.
const PATIENCE: Duration = Duration::from_secs(10);

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: record <fixture.parity>...");
        std::process::exit(2);
    };
    let paths: Vec<PathBuf> = std::iter::once(path)
        .chain(args.map(PathBuf::from))
        .collect();

    for path in &paths {
        match record(path) {
            Ok(()) => println!("recorded {}", path.display()),
            Err(err) => {
                eprintln!("{}: {err}", path.display());
                std::process::exit(1);
            }
        }
    }
}

fn record(path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("cannot read: {err}"))?;
    let script = match Fixture::parse(&text) {
        Ok(fixture) => fixture.script(),
        // A fixture being written for the first time holds commands and nothing else.
        Err(_) => text.clone(),
    };
    let commands = parse_script(&script)?;

    let mut sway = Sway::start()?;
    let result = sway.run(&commands);
    sway.stop();
    let steps = result?;

    let fixture = Fixture {
        source: version()?,
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

fn version() -> Result<String, String> {
    let out = Command::new("sway")
        .arg("--version")
        .output()
        .map_err(|err| format!("cannot run sway: {err}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// A directory that cleans itself up, so a failed recording leaves nothing behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("tiri-parity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)
            .map_err(|err| format!("cannot create a working directory: {err}"))?;
        Ok(TempDir(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Sway {
    process: Child,
    socket: PathBuf,
    _dir: TempDir,
    /// Which window each sway node is, numbered by the order the script opened them.
    order: sway::OpenOrder,
    /// Windows opened so far. Never decremented: closing window 1 does not make the next
    /// window window 1 again, and the tiri replayer numbers them the same way.
    opened: u32,
}

impl Sway {
    fn start() -> Result<Self, String> {
        let dir = TempDir::new()?;
        let config = dir.0.join("config");
        // Pinned bare: gaps, borders and title bars are configuration, and the model
        // compares fractions of the working area, so any of them would show up as a
        // difference that says nothing about behaviour.
        std::fs::write(
            &config,
            format!(
                "output HEADLESS-1 mode {}x{}\n\
                 default_border none\n\
                 default_floating_border none\n\
                 gaps inner 0\n\
                 gaps outer 0\n\
                 focus_follows_mouse no\n",
                OUTPUT.0, OUTPUT.1
            ),
        )
        .map_err(|err| format!("cannot write the sway config: {err}"))?;

        let socket = dir.0.join("sway.sock");
        let process = Command::new("sway")
            .arg("-c")
            .arg(&config)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("SWAYSOCK", &socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("cannot start sway: {err}"))?;

        let started = Instant::now();
        while !socket.exists() {
            if started.elapsed() > PATIENCE {
                return Err("sway did not come up".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let sway = Sway {
            process,
            socket,
            _dir: dir,
            order: sway::OpenOrder::new(),
            opened: 0,
        };
        // The socket exists before the output does; wait for a tree we can read.
        let started = Instant::now();
        while sway.tree().is_err() {
            if started.elapsed() > PATIENCE {
                return Err("sway came up but never answered".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(sway)
    }

    fn stop(&mut self) {
        let _ = self.msg(&["exit"]);
        let _ = self.process.wait();
    }

    fn msg(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new("swaymsg")
            .env("SWAYSOCK", &self.socket)
            .args(args)
            .output()
            .map_err(|err| format!("cannot run swaymsg: {err}"))?;
        if !out.status.success() {
            return Err(format!(
                "swaymsg {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn tree(&self) -> Result<String, String> {
        self.msg(&["-t", "get_tree"])
    }

    fn run(&mut self, commands: &[String]) -> Result<Vec<Step>, String> {
        let mut steps = Vec::with_capacity(commands.len());
        for command in commands {
            match command.as_str() {
                "open" => self.open()?,
                "close" => self.close()?,
                other => {
                    self.msg(&[other])?;
                }
            }
            steps.push(Step {
                command: command.clone(),
                model: self.observe()?,
            });
        }
        Ok(steps)
    }

    /// Spawn a client and wait for it to map, remembering which node it became.
    ///
    /// The window's identity in the model is the order it was opened in, so this is the
    /// only place identity is established — and a client that never maps has to be an
    /// error, since a missing window would otherwise read as agreement.
    fn open(&mut self) -> Result<(), String> {
        let before = self.leaves()?;
        // Fixed size on purpose: sway gives a window floated out of tiling the size its
        // client asked for, so a client whose size depends on fonts or terminal defaults
        // would make the recording depend on the machine it was made on.
        let client = std::env::var("TIRI_PARITY_CLIENT").unwrap_or_else(|_| {
            "foot --window-size-pixels=400x300 sh -c 'while :; do sleep 1; done'".to_owned()
        });
        self.msg(&["exec", &client])?;

        let started = Instant::now();
        loop {
            let now = self.leaves()?;
            if let Some(&new) = now.difference(&before).next() {
                self.opened += 1;
                self.order.insert(new, self.opened);
                return Ok(());
            }
            if started.elapsed() > PATIENCE {
                return Err(format!("the client never mapped: {client}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn close(&mut self) -> Result<(), String> {
        let before = self.leaves()?;
        self.msg(&["kill"])?;

        let started = Instant::now();
        loop {
            let now = self.leaves()?;
            if let Some(&gone) = before.difference(&now).next() {
                self.order.remove(&gone);
                return Ok(());
            }
            if started.elapsed() > PATIENCE {
                return Err("the window never went away".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn leaves(&self) -> Result<HashSet<i64>, String> {
        let tree = self.tree()?;
        let value: serde_json::Value =
            serde_json::from_str(&tree).map_err(|err| format!("unreadable tree: {err}"))?;
        let mut out = HashSet::new();
        collect_leaves(&value, &mut out);
        Ok(out)
    }

    fn observe(&self) -> Result<tiri_parity::Workspace, String> {
        // Recorded raw: what counts as decoration is a rule in this crate, and baking it
        // into the files would mean every improvement to it needs a machine with sway.
        sway::normalize(&self.tree()?, &self.order)
            .map_err(|err| format!("cannot normalize sway's tree: {err:?}"))
    }
}

fn collect_leaves(node: &serde_json::Value, out: &mut HashSet<i64>) {
    let children: Vec<&serde_json::Value> = ["nodes", "floating_nodes"]
        .iter()
        .filter_map(|key| node.get(key))
        .filter_map(|value| value.as_array())
        .flatten()
        .collect();

    let kind = node.get("type").and_then(|value| value.as_str());
    if children.is_empty() && matches!(kind, Some("con") | Some("floating_con")) {
        if let Some(id) = node.get("id").and_then(|value| value.as_i64()) {
            out.insert(id);
        }
        return;
    }
    for child in children {
        collect_leaves(child, out);
    }
}
