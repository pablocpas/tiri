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
use std::path::PathBuf;

use super::known::{self, Divergence, Verdict, KNOWN};
use super::replay;

fn fixtures() -> Vec<PathBuf> {
    let dir = known::fixtures_dir();
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
        known::fixtures_dir().display()
    );
}

#[test]
fn tiri_matches_every_recorded_sway_session() {
    let mut report = String::new();
    let mut known_report = String::new();
    let mut seen: Vec<(String, usize)> = Vec::new();

    for path in fixtures() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let fixture = known::read(&path);

        let actual = replay(&fixture.script(), fixture.client);
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
            let (expected, got) = known::compare(&recorded.model, &replayed.model);
            let differences = expected.diff(&got);
            if differences.is_empty() {
                continue;
            }
            if let Some(known) = KNOWN
                .iter()
                .find(|entry| entry.fixture == name && entry.step == step + 1)
            {
                let label = match known.verdict {
                    Verdict::Open => "known, still open",
                    Verdict::Deliberate => "known, deliberate",
                };
                let _ = writeln!(
                    known_report,
                    "\n{name}, step {} — after `{}`: {label}.\n  {}",
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
        eprintln!("known divergences:{known_report}");
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

    // A deliberate entry that stopped diverging is not progress: tiri drifted towards a
    // behaviour it had decided against, and nobody chose that. Say which kind before saying
    // what to do about it.
    let (settled, drifted): (Vec<&Divergence>, Vec<&Divergence>) = stale
        .into_iter()
        .partition(|entry| entry.verdict == Verdict::Open);
    assert!(
        drifted.is_empty(),
        "these entries in KNOWN are deliberate but no longer diverge — tiri now matches sway \
         where it had decided not to, which is a regression unless the decision changed: {:?}",
        drifted
            .iter()
            .map(|entry| (entry.fixture, entry.step))
            .collect::<Vec<_>>()
    );
    assert!(
        settled.is_empty(),
        "these entries in KNOWN no longer diverge and should be deleted: {:?}",
        settled
            .iter()
            .map(|entry| (entry.fixture, entry.step))
            .collect::<Vec<_>>()
    );
}
