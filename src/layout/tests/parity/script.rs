//! The script format, and the one place the two command vocabularies meet.
//!
//! A script is plain text, one command per line, written in i3's own command grammar. That
//! grammar is the contract both compositors already implement, so a script is reviewable in
//! a diff and can be fed to `swaymsg` verbatim when recording ground truth.
//!
//! Only `open` and `close` are ours: sway spawns clients out of band, so the harness has to
//! own window creation in order to give windows the stable identities the model compares by.

use std::fmt;

use tiri_ipc::SizeChange;

use crate::layout::tests::{Op, TestWindowParams};
use crate::layout::Direction;

/// One line of a script: the text as written, and what tiri does with it.
pub(crate) struct Step {
    /// The command as written, kept verbatim so failures quote the script, not an `Op`.
    pub command: String,
    pub op: Op,
}

#[derive(Debug)]
pub(crate) struct ParseError {
    pub line: usize,
    pub command: String,
    pub reason: Reason,
}

#[derive(Debug)]
pub(crate) enum Reason {
    /// No `Op` implements this command yet.
    Unsupported,
    /// The command exists in i3 but this argument spelling does not.
    BadArgument,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.reason {
            Reason::Unsupported => "no Op implements this command",
            Reason::BadArgument => "unknown argument",
        };
        write!(f, "line {}: {:?}: {what}", self.line, self.command)
    }
}

/// Parse a script into the steps a replay will apply.
///
/// Blank lines and `#` comments are dropped. Window ids are assigned here rather than at
/// replay time so that a parsed script fully determines which window is which.
pub(crate) fn parse(text: &str, client: (i32, i32)) -> Result<Vec<Step>, ParseError> {
    let mut steps = Vec::new();
    let mut next_id = 1;

    for (idx, raw) in text.lines().enumerate() {
        let command = raw.split('#').next().unwrap_or("").trim();
        if command.is_empty() {
            continue;
        }

        let op = match op_for(command, &mut next_id, client) {
            Ok(op) => op,
            Err(reason) => {
                return Err(ParseError {
                    line: idx + 1,
                    command: command.to_owned(),
                    reason,
                });
            }
        };
        steps.push(Step {
            command: command.to_owned(),
            op,
        });
    }

    Ok(steps)
}

/// The vocabulary table.
///
/// Deliberately exhaustive rather than clever: every entry is a claim that tiri's `Op` means
/// what i3 documents the command to mean, and a wrong claim here would make every script
/// silently test the wrong thing. Commands are added as scripts need them; an unknown one is
/// an error, never a no-op, so a typo cannot quietly weaken a test.
fn op_for(command: &str, next_id: &mut usize, client: (i32, i32)) -> Result<Op, Reason> {
    let words: Vec<&str> = command.split_whitespace().collect();

    Ok(match words.as_slice() {
        // Harness pseudo-commands.
        ["open"] => {
            let id = *next_id;
            *next_id += 1;
            Op::AddWindow {
                params: TestWindowParams::mapped_at(id, client),
            }
        }
        ["close"] => Op::CloseFocused,

        // sway's `resize grow|shrink <axis> <amount> px`, which is `resize_tiled`: it finds
        // the nearest ancestor laid out along the axis and moves the focused node's share
        // inside it, taking the difference from the siblings. `set_window_width/height` is
        // that same sentence, which is why they are claimed equal here.
        //
        // Only `px`. sway's `ppt` is a percentage of the *parent's* extent and tiri's
        // `AdjustProportion` is a fraction of the working area, so claiming those equal would
        // be claiming something untrue about every nested container.
        // sway's `resize set`, which works out the delta and hands it to the same
        // `container_resize_tiled` the adjust forms use.
        ["resize", "set", axis @ ("width" | "height"), amount, "px"] => {
            let amount: i32 = amount.parse().map_err(|_| Reason::BadArgument)?;
            let change = SizeChange::SetFixed(amount);
            match *axis {
                "width" => Op::SetWindowWidth { id: None, change },
                _ => Op::SetWindowHeight { id: None, change },
            }
        }

        ["resize", grow_or_shrink @ ("grow" | "shrink"), axis @ ("width" | "height"), amount, "px"] =>
        {
            let amount: i32 = amount.parse().map_err(|_| Reason::BadArgument)?;
            let change = SizeChange::AdjustFixed(if *grow_or_shrink == "shrink" {
                -amount
            } else {
                amount
            });
            match *axis {
                "width" => Op::SetWindowWidth { id: None, change },
                _ => Op::SetWindowHeight { id: None, change },
            }
        }

        // The edge forms: same resize, one payer. sway's `resize grow left` takes from the
        // sibling on the left; the direction names the payer and nothing else.
        ["resize", grow_or_shrink @ ("grow" | "shrink"), edge @ ("left" | "right" | "up" | "down"), amount, "px"] =>
        {
            let amount: i32 = amount.parse().map_err(|_| Reason::BadArgument)?;
            let change = SizeChange::AdjustFixed(if *grow_or_shrink == "shrink" {
                -amount
            } else {
                amount
            });
            let direction = match *edge {
                "left" => Direction::Left,
                "right" => Direction::Right,
                "up" => Direction::Up,
                _ => Direction::Down,
            };
            Op::ResizeWindowEdge {
                id: None,
                change,
                direction,
            }
        }

        ["split", arg] => match *arg {
            "h" | "horizontal" => Op::SplitHorizontal,
            "v" | "vertical" => Op::SplitVertical,
            // Not `layout toggle split`: sway's `split toggle` is a `split`, and wraps.
            "toggle" => Op::SplitToggle,
            _ => return Err(Reason::BadArgument),
        },

        ["layout", arg] => match *arg {
            "splith" => Op::SetLayoutSplitH,
            "splitv" => Op::SetLayoutSplitV,
            "tabbed" => Op::SetLayoutTabbed,
            "stacking" | "stacked" => Op::SetLayoutStacked,
            _ => return Err(Reason::BadArgument),
        },
        ["layout", "toggle", "split"] => Op::ToggleSplitLayout,
        ["layout", "toggle", "all"] => Op::ToggleLayoutAll,

        ["focus", arg] => match *arg {
            "left" => Op::FocusColumnLeft,
            "right" => Op::FocusColumnRight,
            "up" => Op::FocusWindowUp,
            "down" => Op::FocusWindowDown,
            "parent" => Op::FocusParent,
            "child" => Op::FocusChild,
            _ => return Err(Reason::BadArgument),
        },

        ["move", arg] => match *arg {
            "left" => Op::MoveColumnLeft,
            "right" => Op::MoveColumnRight,
            "up" => Op::MoveWindowUp,
            "down" => Op::MoveWindowDown,
            _ => return Err(Reason::BadArgument),
        },

        ["floating", "toggle"] => Op::ToggleWindowFloating { id: None },
        ["fullscreen", "toggle"] => Op::ToggleFullscreenFocused,

        _ => return Err(Reason::Unsupported),
    })
}
