//! Driving a headless sway session.
//!
//! Lives here rather than in the recorder so the differential fuzz can use the same one:
//! both need to ask sway what it does with a script, and neither should be reimplementing
//! how to start it, how to wait for a window, or how to keep window identity straight.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::model::Workspace;
use crate::sway;

/// The output every recording runs on. Must match what the tiri replayer pins, or geometry
/// differences would only be reporting a difference in configuration.
pub const OUTPUT: (u32, u32) = (1280, 720);

/// How long to wait for sway, or for a window to appear or vanish.
const PATIENCE: Duration = Duration::from_secs(10);

pub fn version() -> Result<String, String> {
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

pub struct Sway {
    process: Child,
    socket: PathBuf,
    _dir: TempDir,
    /// Which window each sway node is, numbered by the order the script opened them.
    order: sway::OpenOrder,
    /// Windows opened so far. Never decremented: closing window 1 does not make the next
    /// window window 1 again, and the tiri replayer numbers them the same way.
    opened: u32,
    /// Bumped by `reset`, so each script gets a workspace nothing has touched.
    workspace: u32,
}

impl Sway {
    pub fn start() -> Result<Self, String> {
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
            workspace: 0,
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

    /// Start a fresh workspace, so the next script does not inherit this one's tree.
    ///
    /// Cheaper than restarting sway by the couple of seconds it takes to come up, which is
    /// the difference between a fuzz that runs thousands of scripts and one that runs
    /// hundreds.
    pub fn reset(&mut self) -> Result<(), String> {
        for id in self.order.keys().copied().collect::<Vec<_>>() {
            let _ = self.msg(&[&format!("[con_id={id}]"), "kill"]);
        }
        let started = Instant::now();
        while !self.leaves()?.is_empty() {
            if started.elapsed() > PATIENCE {
                return Err("windows from the last script would not go away".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.order.clear();
        self.opened = 0;
        // A workspace keeps its layout after its last window closes, so move to a new one
        // rather than trust the old one to be blank.
        self.workspace += 1;
        let workspace = self.workspace;
        self.msg(&["workspace", &format!("fuzz{workspace}")])?;
        Ok(())
    }

    pub fn stop(&mut self) {
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
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Err(format!("swaymsg {args:?} failed: {detail}"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Send a layout command, tolerating the ones sway declines to act on.
    ///
    /// sway refuses a command that makes no sense in the current state — moving the only
    /// window sideways, say — and that is a fact about the script, not a broken harness:
    /// tiri does nothing there too, so the two still agree. A command sway does not
    /// *recognise* is different, and stays an error, or a typo in a script would quietly
    /// test nothing.
    fn command(&self, command: &str) -> Result<(), String> {
        match self.msg(&[command]) {
            Ok(_) => Ok(()),
            Err(err) if err.contains("Unknown") || err.contains("nvalid") => Err(err),
            Err(_) => Ok(()),
        }
    }

    fn tree(&self) -> Result<String, String> {
        self.msg(&["-t", "get_tree"])
    }

    pub fn run(&mut self, commands: &[String]) -> Result<Vec<(String, Workspace)>, String> {
        let mut steps = Vec::with_capacity(commands.len());
        for command in commands {
            match command.as_str() {
                "open" => self.open()?,
                "close" => self.close()?,
                other => self.command(other)?,
            }
            steps.push((command.clone(), self.observe()?));
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

    pub fn observe(&self) -> Result<Workspace, String> {
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
