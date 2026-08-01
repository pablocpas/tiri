//! Replay every recorded fixture against tiri and compare, command by command.
//!
//! The fixtures under `tiri-parity/fixtures` are what sway actually did; see
//! `docs/design/parity.md`. Recording needs sway installed, this comparison does not, so it
//! runs everywhere.
//!
//! A divergence is reported at the command that caused it, with both models printed in full,
//! because the useful question is never "did the whole script match" but "which command
//! stopped matching and how".

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use tiri_parity::{erase_decoration, Fixture};

use super::replay;

/// Divergences that are known, understood, and not yet closed.
///
/// An entry silences one fixture from the step it names onwards — everything after a
/// divergence describes a state that already went wrong. The steps before it are still
/// compared, and other fixtures are untouched. Without this the choice would be between a
/// red suite and deleting the fixture that found the problem, and both of those end with
/// nobody recording anything.
///
/// Adding an entry is a claim that the difference is understood. Removing one is the point.
const KNOWN: &[Divergence] = &[
    Divergence {
        fixture: "floating.parity",
        step: 3,
        reason: "A window's size when it starts floating is the size its client asked for, and the two harnesses run different clients — foot under sway, a synthetic window under tiri. This is a limitation of the comparison, not a difference in tiri: closing it needs a recorder client that requests a fixed size.",
    },
    Divergence {
        fixture: "move-and-focus.parity",
        step: 13,
        reason: "`move down` on a window whose parent is the workspace: sway turns the workspace vertical, wraps what was there in a horizontal container and puts the moved window beside it. tiri keeps one flat row and reorders inside it. A real difference in tiri, in the same family as the layout rules already fixed — a command aimed at a node whose parent is the workspace has to build a container rather than change the workspace.",
    },
    Divergence {
        fixture: "tabbed-visibility.parity",
        step: 9,
        reason: "Closing tabs down to the last one: sway keeps the tabbed container under the workspace, tiri promotes it and the workspace itself becomes tabbed. The cleanup pass still collapses a single-child root, which erases the level the workspace occupies. A real difference in tiri.",
    },
];

struct Divergence {
    fixture: &'static str,
    /// 1-based, counting the commands in the fixture.
    step: usize,
    reason: &'static str,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tiri-parity/fixtures")
}

fn fixtures() -> Vec<PathBuf> {
    let dir = fixtures_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()))
        .map(|entry| entry.expect("cannot read a fixture entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "parity"))
        .collect();
    paths.sort();
    paths
}

#[test]
fn there_is_something_to_compare_against() {
    // An empty directory would make every other test in this file pass by doing nothing.
    assert!(
        !fixtures().is_empty(),
        "no fixtures found in {}",
        fixtures_dir().display()
    );
}

#[test]
fn tiri_matches_every_recorded_sway_session() {
    let mut report = String::new();
    let mut known_report = String::new();
    let mut seen: Vec<(String, usize)> = Vec::new();

    for path in fixtures() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).expect("cannot read the fixture");
        let fixture = Fixture::parse(&text)
            .unwrap_or_else(|err| panic!("{name} is not a readable fixture: {err}"));

        let actual = replay(&fixture.script());
        if actual.steps.len() != fixture.steps.len() {
            let _ = writeln!(
                report,
                "\n{name}: replayed {} commands but the recording has {}",
                actual.steps.len(),
                fixture.steps.len()
            );
            continue;
        }

        for (step, (recorded, replayed)) in fixture.steps.iter().zip(&actual.steps).enumerate() {
            // Decoration is erased here rather than at record time, so improving the rule
            // does not invalidate recordings that only a machine with sway could redo.
            let mut expected = recorded.model.clone();
            let mut got = replayed.model.clone();
            erase_decoration(&mut expected);
            erase_decoration(&mut got);

            let differences = expected.diff(&got);
            if differences.is_empty() {
                continue;
            }
            if let Some(known) = KNOWN
                .iter()
                .find(|entry| entry.fixture == name && entry.step == step + 1)
            {
                let _ = writeln!(
                    known_report,
                    "\n{name}, step {} — after `{}`: known.\n  {}",
                    step + 1,
                    recorded.command,
                    known.reason
                );
                seen.push((name.clone(), step + 1));
                // Everything after a divergence is downstream of it, known or not.
                break;
            }

            let _ = writeln!(
                report,
                "\n{name}, step {} — after `{}`:",
                step + 1,
                recorded.command
            );
            for difference in &differences {
                let _ = writeln!(
                    report,
                    "  at {}: sway has {}, tiri has {}",
                    difference.at, difference.expected, difference.actual
                );
            }
            let _ = writeln!(
                report,
                "--- sway ---\n{}--- tiri ---\n{}",
                expected.render(),
                got.render()
            );
            // One report per script: the steps after a divergence describe a state that
            // already went wrong, so listing them adds noise rather than information.
            break;
        }
    }

    if !known_report.is_empty() {
        eprintln!("known divergences, still open:{known_report}");
    }

    assert!(
        report.is_empty(),
        "tiri and sway disagree:\n{report}\n\
         If this is deliberate, add it to KNOWN with the reason. To re-record:\n\
         cargo run -p tiri-parity --bin record -- tiri-parity/fixtures/*.parity"
    );

    let stale: Vec<&Divergence> = KNOWN
        .iter()
        .filter(|entry| {
            !seen
                .iter()
                .any(|(f, s)| f == entry.fixture && *s == entry.step)
        })
        .collect();
    assert!(
        stale.is_empty(),
        "these entries in KNOWN no longer diverge and should be deleted: {:?}",
        stale
            .iter()
            .map(|entry| (entry.fixture, entry.step))
            .collect::<Vec<_>>()
    );
}
