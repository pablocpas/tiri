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
        let mut out = format!("# recorded from {}\n", self.source);
        for step in &self.steps {
            let _ = write!(out, "\n$ {}\n{}", step.command, step.model.render());
        }
        out
    }

    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut source = String::new();
        let mut steps: Vec<Step> = Vec::new();
        let mut pending: Option<(String, usize, String)> = None;

        for (idx, line) in text.lines().enumerate() {
            let no = idx + 1;
            if let Some(rest) = line.strip_prefix("# recorded from ") {
                source = rest.trim().to_owned();
                continue;
            }
            if line.starts_with('#') {
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
        Ok(Fixture { source, steps })
    }
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
