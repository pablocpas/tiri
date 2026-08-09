//! Driving headless sway and i3 sessions.
//!
//! Lives here rather than in the recorder so the differential fuzz can use the same one:
//! both need to ask sway what it does with a script, and neither should be reimplementing
//! how to start it, how to wait for a window, or how to keep window identity straight.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::model::Workspace;
use crate::sway;

/// The output every recording runs on. Must match what the tiri replayer pins, or geometry
/// differences would only be reporting a difference in configuration.
pub const OUTPUT: (u32, u32) = (1280, 720);

/// The size the recorder's client ends up at, and what a fixture without a `# client` stamp
/// is replayed with.
///
/// It matters because sway floats a window at the size it mapped with, so a client size that
/// differs between the two sides would show up as a layout difference. The recorder asks
/// `foot` for 400x300 and foot rounds down to whole character cells, which is why this is not
/// 400x300 — and why [`CLIENT_COMMAND`] pins the font: the cell size is configuration exactly
/// as gaps and borders are, and it was the one piece left to the machine. Unpinned, this
/// moved between one recording session and the next.
///
/// The authority for an individual recording is the stamp in the file, not this.
pub const CLIENT: (i32, i32) = (400, 285);

/// The client the recorder opens windows with.
///
/// Pinned down to the font, for the same reason the sway config is pinned bare: anything left
/// to the machine shows up later as a difference that says nothing about behaviour.
const CLIENT_COMMAND: &str = "foot --config=/dev/null --font=monospace:size=10 \
                              --override=dpi-aware=no --window-size-pixels=400x300 \
                              sh -c 'while :; do sleep 1; done'";

/// How long to wait for sway, or for a window to appear or vanish.
const PATIENCE: Duration = Duration::from_secs(10);

/// Which sway to drive.
///
/// Overridable because a question about sway's behaviour is sometimes a question about a
/// *particular* sway: a patched build, or one from before a fix. `swaymsg` follows the same
/// rule, since a build being tested may speak a newer IPC than the installed client.
fn sway_binary() -> String {
    std::env::var("TIRI_PARITY_SWAY").unwrap_or_else(|_| "sway".to_owned())
}

fn swaymsg_binary() -> String {
    std::env::var("TIRI_PARITY_SWAYMSG").unwrap_or_else(|_| "swaymsg".to_owned())
}

fn i3_binary() -> String {
    std::env::var("TIRI_PARITY_I3").unwrap_or_else(|_| "i3".to_owned())
}

fn i3msg_binary() -> String {
    std::env::var("TIRI_PARITY_I3MSG").unwrap_or_else(|_| "i3-msg".to_owned())
}

fn is_command_syntax_error(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("unknown") || lower.contains("invalid") || lower.contains("expected")
}

pub fn version() -> Result<String, String> {
    let out = Command::new(sway_binary())
        .arg("--version")
        .output()
        .map_err(|err| format!("cannot run sway: {err}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

pub fn i3_version() -> Result<String, String> {
    let out = Command::new(i3_binary())
        .arg("-v")
        // i3 initializes its IPC path before handling some command-line options.
        .env("XDG_RUNTIME_DIR", std::env::temp_dir())
        .output()
        .map_err(|err| format!("cannot run i3: {err}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// A directory that cleans itself up, so a failed recording leaves nothing behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(backend: &str) -> Result<Self, String> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tiri-parity-{backend}-{}-{serial}",
            std::process::id()
        ));
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

/// A child process that is always reaped, including on partially failed session startup.
struct ManagedChild(Child);

impl ManagedChild {
    fn spawn(command: &mut Command, description: &str) -> Result<Self, String> {
        command
            .spawn()
            .map(Self)
            .map_err(|err| format!("cannot start {description}: {err}"))
    }

    fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.0.stdout.take()
    }

    /// Give a compositor time to honour its exit command, then force termination.
    fn stop(&mut self) {
        let started = Instant::now();
        loop {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if started.elapsed() <= PATIENCE => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) | Err(_) => {
                    self.kill();
                    return;
                }
            }
        }
    }

    fn kill(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct Sway {
    process: ManagedChild,
    socket: PathBuf,
    stderr_log: PathBuf,
    _dir: TempDir,
    /// Which window each sway node is, numbered by the order the script opened them.
    order: sway::OpenOrder,
    /// Windows opened so far. Never decremented: closing window 1 does not make the next
    /// window window 1 again, and the tiri replayer numbers them the same way.
    opened: u32,
    /// Bumped by `reset`, so each script gets a workspace nothing has touched.
    workspace: u32,
    /// The size the clients of this session map at, as observed rather than assumed.
    client: Option<(i32, i32)>,
}

impl Sway {
    pub fn start() -> Result<Self, String> {
        let dir = TempDir::new("sway")?;
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
                 focus_follows_mouse no\n\
                 font pango:sans 12px\n\
                 titlebar_padding 8 3\n\
                 titlebar_border_thickness 0\n",
                OUTPUT.0, OUTPUT.1
            ),
        )
        .map_err(|err| format!("cannot write the sway config: {err}"))?;

        let socket = dir.0.join("sway.sock");
        let stderr_log = dir.0.join("sway.stderr");
        let stderr = std::fs::File::create(&stderr_log)
            .map_err(|err| format!("cannot create sway's error log: {err}"))?;
        let process = ManagedChild::spawn(
            Command::new(sway_binary())
                .arg("-c")
                .arg(&config)
                .env("WLR_BACKENDS", "headless")
                .env("WLR_LIBINPUT_NO_DEVICES", "1")
                .env("SWAYSOCK", &socket)
                .stdout(Stdio::null())
                .stderr(Stdio::from(stderr)),
            "sway",
        )?;

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
            stderr_log,
            _dir: dir,
            order: sway::OpenOrder::new(),
            opened: 0,
            workspace: 0,
            client: None,
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
        wait_for_empty(
            || self.leaves(),
            "windows from the last script would not go away",
        )?;
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
        self.process.stop();
    }

    fn msg(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new(swaymsg_binary())
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
            // A disconnected socket is often the only thing swaymsg can say after sway
            // aborts. Keep the compositor's own last lines beside it; without them a pinned
            // reference crash and a broken IPC harness are indistinguishable.
            let compositor = std::fs::read_to_string(&self.stderr_log).unwrap_or_default();
            let mut tail: Vec<_> = compositor.lines().rev().take(200).collect();
            tail.reverse();
            let compositor = tail.join("\n");
            if compositor.is_empty() {
                return Err(format!("swaymsg {args:?} failed: {detail}"));
            }
            return Err(format!(
                "swaymsg {args:?} failed: {detail}\nsway stderr:\n{compositor}"
            ));
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
            Err(err) if is_command_syntax_error(&err) => Err(err),
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
                "open" => self.open(),
                "close" => self.close(),
                other => self.command(other),
            }
            .map_err(|err| format!("while running `{command}`: {err}"))?;
            let observed = self
                .observe()
                .map_err(|err| format!("after `{command}`: {err}"))?;
            steps.push((command.clone(), observed));
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
        let client =
            std::env::var("TIRI_PARITY_CLIENT").unwrap_or_else(|_| CLIENT_COMMAND.to_owned());
        self.msg(&["exec", &client])?;

        let new = wait_for_new_leaf(
            || self.leaves(),
            &before,
            &format!("the client never mapped: {client}"),
        )?;
        self.opened += 1;
        self.order.insert(new, self.opened);
        self.observe_client_size(new)
    }

    /// Note the size this window mapped at, for the recording to stamp.
    ///
    /// sway reports a view's natural size as its `geometry`, and that is what a window gets
    /// when it starts floating, so a recording is only replayable against a client of the
    /// same size. That is a fact about the recording, so it goes in the file rather than
    /// being asserted against a constant — a machine that produces a different size records
    /// a fixture that says so, instead of being unable to record at all.
    ///
    /// Two windows of *different* sizes inside one recording is the thing that cannot be
    /// stamped, and that is skew rather than a property, so it fails here.
    fn observe_client_size(&mut self, node: i64) -> Result<(), String> {
        let tree = self.tree()?;
        let value: serde_json::Value =
            serde_json::from_str(&tree).map_err(|err| format!("unreadable tree: {err}"))?;
        let Some(geometry) = find_geometry(&value, node) else {
            return Ok(());
        };
        match self.client {
            Some(seen) if seen != geometry => Err(format!(
                "one client mapped at {}x{} and another at {}x{} in the same recording",
                seen.0, seen.1, geometry.0, geometry.1
            )),
            _ => {
                self.client = Some(geometry);
                Ok(())
            }
        }
    }

    /// The size the clients of this session mapped at, once one has.
    pub fn client_size(&self) -> (i32, i32) {
        self.client.unwrap_or(CLIENT)
    }

    /// Close what is focused, and wait for *all* of it to go.
    ///
    /// `kill` closes the whole focused subtree, so after a `focus parent` it takes several
    /// windows with it. Returning as soon as the first one vanished would sample a tree
    /// mid-teardown: a recording that depends on timing, and fuzz divergences that are not
    /// really there.
    fn close(&mut self) -> Result<(), String> {
        let before = self.leaves()?;
        self.msg(&["kill"])?;

        let remaining = wait_for_close(|| self.leaves(), &before, "the window never went away")?;
        for gone in before.difference(&remaining) {
            self.order.remove(gone);
        }
        Ok(())
    }

    fn leaves(&self) -> Result<HashSet<i64>, String> {
        leaves_from_tree(&self.tree()?, "sway")
    }

    pub fn observe(&self) -> Result<Workspace, String> {
        // Recorded raw: what counts as decoration is a rule in this crate, and baking it
        // into the files would mean every improvement to it needs a machine with sway.
        let tree = self.tree()?;
        normalize_tree(&tree, &self.order, "sway")
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        self.stop();
    }
}

/// An i3 session running on its own Xvfb display.
///
/// This is intentionally independent of the user's X session: neither i3 nor the test
/// clients ever receive its `DISPLAY`. The observable tree uses the same IPC schema and
/// normalizer as sway, which is one of i3's compatibility contracts.
pub struct I3 {
    process: ManagedChild,
    xvfb: ManagedChild,
    display: String,
    socket: PathBuf,
    _dir: TempDir,
    order: sway::OpenOrder,
    opened: u32,
    workspace: u32,
}

impl I3 {
    pub fn start() -> Result<Self, String> {
        let dir = TempDir::new("i3")?;

        // Let Xvfb select a free display atomically. Scanning /tmp/.X11-unix first has a
        // race, and a parity runner is useful precisely when several copies can run in CI.
        let mut xvfb = ManagedChild::spawn(
            Command::new("Xvfb")
                .args([
                    "-displayfd",
                    "1",
                    "-screen",
                    "0",
                    &format!("{}x{}x24", OUTPUT.0, OUTPUT.1),
                    "-nolisten",
                    "tcp",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
            "Xvfb",
        )?;
        let mut number = String::new();
        let stdout = xvfb
            .stdout()
            .ok_or_else(|| "cannot read Xvfb's display number".to_owned())?;
        BufReader::new(stdout)
            .read_line(&mut number)
            .map_err(|err| format!("cannot read Xvfb's display number: {err}"))?;
        let number = number.trim();
        if number.is_empty() {
            return Err("Xvfb exited without allocating a display".into());
        }
        let display = format!(":{number}");

        let socket = dir.0.join("i3.sock");
        let config = dir.0.join("config");
        std::fs::write(
            &config,
            format!(
                "font pango:monospace 8\n\
                 default_border none\n\
                 default_floating_border none\n\
                 focus_follows_mouse no\n\
                 workspace_layout default\n\
                 ipc-socket {}\n",
                socket.display()
            ),
        )
        .map_err(|err| format!("cannot write the i3 config: {err}"))?;

        let process = ManagedChild::spawn(
            Command::new(i3_binary())
                .arg("-a")
                .arg("-c")
                .arg(&config)
                .env("DISPLAY", &display)
                .env("XDG_RUNTIME_DIR", &dir.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
            "i3",
        )?;

        let started = Instant::now();
        while !socket.exists() {
            if started.elapsed() > PATIENCE {
                return Err("i3 did not come up".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let i3 = I3 {
            process,
            xvfb,
            display,
            socket,
            _dir: dir,
            order: sway::OpenOrder::new(),
            opened: 0,
            workspace: 0,
        };
        let started = Instant::now();
        while i3.tree().is_err() {
            if started.elapsed() > PATIENCE {
                return Err("i3 came up but never answered".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(i3)
    }

    pub fn reset(&mut self) -> Result<(), String> {
        for id in self.order.keys().copied().collect::<Vec<_>>() {
            let _ = self.msg(&format!("[con_id={id}] kill"), "command");
        }
        wait_for_empty(
            || self.leaves(),
            "windows from the last i3 script would not go away",
        )?;
        self.order.clear();
        self.opened = 0;
        self.workspace += 1;
        self.command(&format!("workspace fuzz{}", self.workspace))
    }

    pub fn stop(&mut self) {
        let _ = self.msg("exit", "command");
        self.process.stop();
        self.xvfb.kill();
    }

    fn msg(&self, message: &str, kind: &str) -> Result<String, String> {
        let out = Command::new(i3msg_binary())
            .arg("-s")
            .arg(&self.socket)
            .arg("-t")
            .arg(kind)
            .arg(message)
            .env("DISPLAY", &self.display)
            .env("XDG_RUNTIME_DIR", &self._dir.0)
            .output()
            .map_err(|err| format!("cannot run i3-msg: {err}"))?;
        if !out.status.success() {
            // i3-msg exits non-zero when i3 understood a layout command but could not
            // apply it (for example, resizing a lone tiled window). The JSON reply is the
            // protocol result, and `command` below decides whether it is a harmless no-op
            // or a malformed command. Transport/query failures still remain errors here.
            if kind == "command" && serde_json::from_slice::<serde_json::Value>(&out.stdout).is_ok()
            {
                return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Err(format!("i3-msg `{message}` failed: {detail}"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn command(&self, command: &str) -> Result<(), String> {
        let reply = self.msg(command, "command")?;
        let values: serde_json::Value = serde_json::from_str(&reply)
            .map_err(|err| format!("unreadable i3 command reply: {err}"))?;
        let failed = values.as_array().is_some_and(|results| {
            results
                .iter()
                .any(|result| result.get("success").and_then(|v| v.as_bool()) == Some(false))
        });
        if failed && is_command_syntax_error(&reply) {
            return Err(format!("i3 rejected `{command}`: {reply}"));
        }
        Ok(())
    }

    fn tree(&self) -> Result<String, String> {
        self.msg("", "get_tree")
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

    fn open(&mut self) -> Result<(), String> {
        let before = self.leaves()?;
        let next = self.opened + 1;
        let title = format!("tiri-parity-{next}");
        self.command(&format!(
            "exec --no-startup-id xmessage -name {title} -title {title} {title}"
        ))?;

        let new = wait_for_new_leaf(
            || self.leaves(),
            &before,
            "the X11 test client never mapped",
        )?;
        self.opened = next;
        self.order.insert(new, next);
        Ok(())
    }

    fn close(&mut self) -> Result<(), String> {
        let before = self.leaves()?;
        self.command("kill")?;

        let remaining = wait_for_close(
            || self.leaves(),
            &before,
            "the X11 test client never went away",
        )?;
        for gone in before.difference(&remaining) {
            self.order.remove(gone);
        }
        Ok(())
    }

    fn leaves(&self) -> Result<HashSet<i64>, String> {
        leaves_from_tree(&self.tree()?, "i3")
    }

    pub fn observe(&self) -> Result<Workspace, String> {
        normalize_tree(&self.tree()?, &self.order, "i3")
    }

    /// The X11 client's natural size is irrelevant while it remains tiled. Returning the
    /// recorder's pinned size lets the same tiri replayer drive tiling-only comparisons.
    pub fn client_size(&self) -> (i32, i32) {
        CLIENT
    }
}

fn wait_for_empty(
    mut leaves: impl FnMut() -> Result<HashSet<i64>, String>,
    timeout: &str,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        if leaves()?.is_empty() {
            return Ok(());
        }
        if started.elapsed() > PATIENCE {
            return Err(timeout.to_owned());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_new_leaf(
    mut leaves: impl FnMut() -> Result<HashSet<i64>, String>,
    before: &HashSet<i64>,
    timeout: &str,
) -> Result<i64, String> {
    let started = Instant::now();
    loop {
        if let Some(new) = leaves()?.difference(before).next().copied() {
            return Ok(new);
        }
        if started.elapsed() > PATIENCE {
            return Err(timeout.to_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_close(
    mut leaves: impl FnMut() -> Result<HashSet<i64>, String>,
    before: &HashSet<i64>,
    timeout: &str,
) -> Result<HashSet<i64>, String> {
    let started = Instant::now();
    let mut last_change = Instant::now();
    let mut seen = before.clone();
    loop {
        let now = leaves()?;
        if now != seen {
            seen = now;
            last_change = Instant::now();
        }
        // A focused container can close several clients; wait until the cascade settles.
        if seen.len() < before.len() && last_change.elapsed() > Duration::from_millis(150) {
            return Ok(seen);
        }
        if started.elapsed() > PATIENCE {
            return Err(timeout.to_owned());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn leaves_from_tree(tree: &str, backend: &str) -> Result<HashSet<i64>, String> {
    let value: serde_json::Value =
        serde_json::from_str(tree).map_err(|err| format!("unreadable {backend} tree: {err}"))?;
    let mut out = HashSet::new();
    collect_leaves(&value, &mut out);
    Ok(out)
}

fn normalize_tree(tree: &str, order: &sway::OpenOrder, backend: &str) -> Result<Workspace, String> {
    sway::normalize(tree, order)
        .map_err(|err| format!("cannot normalize {backend}'s tree: {err:?}"))
}

impl Drop for I3 {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The natural size sway reports for one node, which is the size its client mapped with.
fn find_geometry(node: &serde_json::Value, id: i64) -> Option<(i32, i32)> {
    if node.get("id").and_then(serde_json::Value::as_i64) == Some(id) {
        let geometry = node.get("geometry")?;
        let width = geometry.get("width")?.as_i64()? as i32;
        let height = geometry.get("height")?.as_i64()? as i32;
        return Some((width, height));
    }
    ["nodes", "floating_nodes"]
        .iter()
        .filter_map(|key| node.get(key))
        .filter_map(|value| value.as_array())
        .flatten()
        .find_map(|child| find_geometry(child, id))
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
