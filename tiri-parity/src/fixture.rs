//! What a recording against sway looks like on disk.
//!
//! One file per scenario, holding the script and what sway did after each of its commands.
//! The script is not stored separately: every `$ ` line is a command, so the file is both
//! the input and the expectation and the two cannot drift apart.
//!
//! The format is the same rendering used in failure messages, so a re-recording after a sway
//! upgrade shows up as an ordinary reviewable diff — that diff is the interesting artefact,
//! because it says what changed in sway.

use std::fmt::Write as _;

use crate::model::{self, Workspace};

/// A recorded scenario.
#[derive(Debug)]
pub struct Fixture {
    /// The compositor and version the recording came from, e.g. `sway 1.11`.
    pub source: String,
    /// What this recording is for, in the words of whoever wrote the script.
    ///
    /// Kept because re-recording rewrites the file, and a recorder that only knows how to
    /// write what it measured deletes the one part of a fixture that says why it exists.
    pub notes: Vec<String>,
    /// The size the client mapped at while this was recorded.
    ///
    /// A property of the recording, like the sway version beside it. sway floats a window at
    /// the size it mapped with, so a replayer whose windows are a different size would report
    /// that as a layout difference — and a corpus recorded across two machines would have no
    /// way to say so. Stamped here, each file replays against the client it was made with.
    pub client: (i32, i32),
    pub steps: Vec<Step>,
}

#[derive(Debug)]
pub struct Step {
    /// The command as written in the script.
    pub command: String,
    /// What the workspace looked like once the command settled.
    pub model: Workspace,
}

#[derive(Debug)]
pub struct ParseError {
    pub line: usize,
    pub reason: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.reason)
    }
}

impl Fixture {
    /// The script alone, ready to hand to a replayer.
    pub fn script(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            let _ = writeln!(out, "{}", step.command);
        }
        out
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for note in &self.notes {
            let _ = writeln!(out, "# {note}");
        }
        let _ = writeln!(out, "# recorded from {}", self.source);
        let _ = writeln!(out, "# client {}x{}", self.client.0, self.client.1);
        for step in &self.steps {
            let _ = write!(out, "\n$ {}\n{}", step.command, step.model.render());
        }
        out
    }

    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut source = String::new();
        // A fixture written before the stamp existed, or one written by hand as a bare
        // script. Only a recording in which a window floats can observe the client's size at
        // all, and those are re-recorded with the stamp; for the rest the default is the
        // same answer by another route.
        let mut client = crate::session::CLIENT;
        let mut notes: Vec<String> = Vec::new();
        let mut steps: Vec<Step> = Vec::new();
        let mut pending: Option<(String, usize, String)> = None;

        for (idx, line) in text.lines().enumerate() {
            let no = idx + 1;
            if let Some(rest) = line.strip_prefix("# recorded from ") {
                source = rest.trim().to_owned();
                continue;
            }
            if let Some(rest) = line.strip_prefix("# client ") {
                client = parse_client(rest).ok_or_else(|| ParseError {
                    line: no,
                    reason: format!("cannot read a client size from {:?}", rest.trim()),
                })?;
                continue;
            }
            if let Some(rest) = line.strip_prefix('#') {
                notes.push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
                continue;
            }
            if let Some(command) = line.strip_prefix("$ ") {
                if let Some(step) = pending.take() {
                    steps.push(finish(step)?);
                }
                pending = Some((command.trim().to_owned(), no, String::new()));
                continue;
            }
            match pending.as_mut() {
                Some((_, _, body)) => {
                    body.push_str(line);
                    body.push('\n');
                }
                None if line.trim().is_empty() => {}
                None => {
                    return Err(ParseError {
                        line: no,
                        reason: "model text before any command".into(),
                    })
                }
            }
        }
        if let Some(step) = pending.take() {
            steps.push(finish(step)?);
        }

        if source.is_empty() {
            return Err(ParseError {
                line: 1,
                reason: "missing the `# recorded from ...` header".into(),
            });
        }
        // A trailing blank comment line is separator rather than prose.
        while notes.last().is_some_and(|note| note.trim().is_empty()) {
            notes.pop();
        }

        Ok(Fixture {
            source,
            notes,
            client,
            steps,
        })
    }
}

/// The prose comments in a fixture, without the stamps the recorder writes itself.
///
/// Separate from [`Fixture::parse`] because a fixture being written for the first time holds
/// commands and nothing else, so it does not parse — and that is exactly when its notes have
/// only ever been typed once and are easiest to lose.
pub fn notes_in(text: &str) -> Vec<String> {
    let mut notes: Vec<String> = text
        .lines()
        .filter_map(|line| line.strip_prefix('#'))
        .filter(|note| {
            let note = note.trim_start();
            !note.starts_with("recorded from ") && !note.starts_with("client ")
        })
        .map(|note| note.strip_prefix(' ').unwrap_or(note).to_owned())
        .collect();
    while notes.last().is_some_and(|note| note.trim().is_empty()) {
        notes.pop();
    }
    notes
}

/// `396x288`, the way the stamp writes it.
fn parse_client(text: &str) -> Option<(i32, i32)> {
    let (width, height) = text.trim().split_once('x')?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

fn finish((command, at, body): (String, usize, String)) -> Result<Step, ParseError> {
    let model = model::parse(&body).map_err(|err| ParseError {
        // The model parser counts from the start of its own text, which begins after the
        // command line.
        line: at + err.line,
        reason: format!("in the model after {command:?}: {}", err.reason),
    })?;
    Ok(Step { command, model })
}
