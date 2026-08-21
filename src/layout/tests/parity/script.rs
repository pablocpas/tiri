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
use crate::layout::MarkMode;
use crate::layout::{ContainerLayout, Direction, LayoutCycleEntry};

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
        // The unit is lexical, exactly as it is in sway: `px` is pixels and `ppt` is
        // hundredths of whatever holds the target. Nothing infers it from the magnitude.
        ["resize", "set", axis @ ("width" | "height"), amount, unit @ ("px" | "ppt")] => {
            let change = match *unit {
                "px" => SizeChange::SetFixed(amount.parse().map_err(|_| Reason::BadArgument)?),
                _ => SizeChange::SetProportion(amount.parse().map_err(|_| Reason::BadArgument)?),
            };
            match *axis {
                "width" => Op::SetWindowWidth { id: None, change },
                _ => Op::SetWindowHeight { id: None, change },
            }
        }

        ["resize", grow_or_shrink @ ("grow" | "shrink"), axis @ ("width" | "height"), amount, unit @ ("px" | "ppt")] =>
        {
            let sign = if *grow_or_shrink == "shrink" { -1. } else { 1. };
            let amount: f64 = amount.parse().map_err(|_| Reason::BadArgument)?;
            let change = match *unit {
                "px" => SizeChange::AdjustFixed((sign * amount) as i32),
                _ => SizeChange::AdjustProportion(sign * amount),
            };
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
            let amount = if *grow_or_shrink == "shrink" {
                -amount
            } else {
                amount
            };
            let direction = match *edge {
                "left" => Direction::Left,
                "right" => Direction::Right,
                "up" => Direction::Up,
                _ => Direction::Down,
            };
            Op::ResizeWindowEdge {
                id: None,
                amount,
                direction,
            }
        }

        ["splith"] => Op::SplitHorizontal,
        ["splitv"] => Op::SplitVertical,
        ["splitt"] => Op::SplitToggle,
        ["split", arg] => match *arg {
            "h" | "horizontal" => Op::SplitHorizontal,
            "v" | "vertical" => Op::SplitVertical,
            // Not `layout toggle split`: sway's `split toggle` is a `split`, and wraps.
            "t" | "toggle" => Op::SplitToggle,
            "n" | "none" => Op::SplitNone,
            _ => return Err(Reason::BadArgument),
        },

        ["layout", arg] => match *arg {
            "splith" => Op::SetLayoutSplitH,
            "splitv" => Op::SetLayoutSplitV,
            "tabbed" => Op::SetLayoutTabbed,
            "stacking" => Op::SetLayoutStacked,
            "default" => Op::SetLayoutDefault,
            "toggle" => Op::ToggleSplitLayout,
            _ => return Err(Reason::BadArgument),
        },
        ["layout", "toggle", "split"] => Op::ToggleSplitLayout,
        ["layout", "toggle", "all"] => Op::ToggleLayoutAll,
        ["layout", "toggle", entries @ ..] if entries.len() >= 2 => {
            let cycle = entries
                .iter()
                .filter_map(|entry| match *entry {
                    "split" => Some(LayoutCycleEntry::Split),
                    "splith" => Some(LayoutCycleEntry::Layout(ContainerLayout::SplitH)),
                    "splitv" => Some(LayoutCycleEntry::Layout(ContainerLayout::SplitV)),
                    "tabbed" => Some(LayoutCycleEntry::Layout(ContainerLayout::Tabbed)),
                    "stacking" => Some(LayoutCycleEntry::Layout(ContainerLayout::Stacked)),
                    // sway silently ignores invalid entries in an explicit cycle.
                    _ => None,
                })
                .collect();
            Op::ToggleLayoutCycle { cycle }
        }

        // sway reads the direction off the parent's layout and then does an ordinary
        // directional focus; `sibling` only stops it descending into what it lands on.
        ["focus", step @ ("next" | "prev")] => Op::FocusAlongParent {
            forward: *step == "next",
            descend: true,
        },
        ["focus", step @ ("next" | "prev"), "sibling"] => Op::FocusAlongParent {
            forward: *step == "next",
            descend: false,
        },

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

        // sway's `swap container with id|con_id|mark <arg>`. The script says which window in
        // the only vocabulary it has — the order it opened them — and the recorder turns
        // that into the `con_id` sway knows it by, the same way `open` and `close` are the
        // harness's words for spawning a client and `kill`.
        //
        // What is swapped is the *selection*, so after `focus parent` this trades whole
        // subtrees, which is `cmd_swap` operating on `handler_context.container`.
        ["swap", "container", "with", id] | ["swap", "with", id] => {
            Op::SwapWithWindow(id.parse().map_err(|_| Reason::BadArgument)?)
        }

        // Workspaces are numbered *within the script*: 1 is the one it starts on. sway names
        // them, tiri indexes them per output, and neither name is something a script could
        // write down and still mean the same thing on both sides — the recorder maps this
        // number onto a name unique to the recording, the replayer onto the index.
        //
        // The model only ever renders the focused workspace, so `move to workspace` is
        // observed as the window leaving and `workspace` is how the recording gets to look
        // at where it went.
        //
        // What moves is the selection, so this is the container move rather than the window
        // one: after `focus parent` sway takes the whole subtree with it. And the focus stays
        // where it was — `cmd_move_container` restores it to what the container left behind
        // (`move.c:598`), while tiri's own action follows the window by default.
        ["move", "container", "to", "workspace", target] | ["move", "to", "workspace", target] => {
            Op::MoveContainerToWorkspace(workspace_index(target)?, false)
        }
        ["workspace", target] => Op::FocusWorkspace(workspace_index(target)?),

        // sway's `mark [--add|--replace] [--toggle] <identifier>`, documented at the top of
        // `sway/commands/mark.c`. Bare `mark` is `--replace`. `--add --toggle` is tiri's
        // `Toggle`: take the mark off this window if it has it, otherwise add it without
        // disturbing the others. `--replace --toggle` has no tiri mode and is left unsaid
        // rather than approximated.
        ["mark", name] => Op::MarkFocused {
            mark: (*name).to_owned(),
            mode: MarkMode::Replace,
        },
        ["mark", "--replace", name] => Op::MarkFocused {
            mark: (*name).to_owned(),
            mode: MarkMode::Replace,
        },
        ["mark", "--add", name] => Op::MarkFocused {
            mark: (*name).to_owned(),
            mode: MarkMode::Add,
        },
        ["mark", "--add", "--toggle", name] | ["mark", "--toggle", "--add", name] => {
            Op::MarkFocused {
                mark: (*name).to_owned(),
                mode: MarkMode::Toggle,
            }
        }

        // `unmark <id>` takes that mark off whichever window holds it; bare `unmark` is the
        // sweeping one.
        ["unmark"] => Op::Unmark { mark: None },
        ["unmark", name] => Op::Unmark {
            mark: Some((*name).to_owned()),
        },

        // The addressed swap, in the harness's vocabulary for a window (see above) and in
        // sway's for a mark.
        ["swap", "container", "with", "mark", name] | ["swap", "with", "mark", name] => {
            Op::SwapWithMark((*name).to_owned())
        }

        _ => return Err(Reason::Unsupported),
    })
}

/// The script's own workspace numbering, as a tiri workspace index.
///
/// 1 is where every script starts, so the numbers are 1-based and the indices are not. A
/// script that says `workspace 0` means nothing, and saying nothing is how a typo turns into
/// a test that passes for the wrong reason.
fn workspace_index(target: &str) -> Result<usize, Reason> {
    let number: usize = target.parse().map_err(|_| Reason::BadArgument)?;
    number.checked_sub(1).ok_or(Reason::BadArgument)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn explicit_layout_toggle_requires_at_least_two_entries_like_sway() {
        assert!(parse("layout toggle tabbed", (400, 300)).is_err());
        assert!(parse("layout toggle tabbed stacking", (400, 300)).is_ok());
    }
}
