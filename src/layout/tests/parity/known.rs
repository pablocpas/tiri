//! Divergences that are known, understood, and not yet closed.
//!
//! One table, read by two consumers that need different things from it. The fixture suite
//! needs to know *where* to stop comparing a recording; the fuzz needs to recognise a
//! difference it has already been told about, in a script nobody wrote down. Keeping the
//! table in one place is what stops those two from drifting into separate lists.
//!
//! Adding an entry is a claim that the difference is understood. What to do about it then is
//! a second claim, and the two are not the same: an [`Open`] entry is debt and removing it is
//! the point, while a [`Deliberate`] one records a place where tiri answers differently on
//! purpose and is expected to keep doing so.
//!
//! Reaching for `Deliberate` because a difference is inconvenient is how a parity suite stops
//! meaning anything. It is for a difference that was traced to sway doing something tiri
//! declines to reproduce, with the reason written down.
//!
//! [`Open`]: Verdict::Open
//! [`Deliberate`]: Verdict::Deliberate

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tiri_parity::{erase_decoration, Fixture, Workspace};

use super::replay;

pub(super) struct Divergence {
    pub fixture: &'static str,
    /// 1-based, counting the commands in the fixture.
    pub step: usize,
    pub verdict: Verdict,
    pub reason: &'static str,
}

/// What the project intends to do about a divergence it understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Tiri should match sway here and does not yet. Removing the entry is the goal.
    Open,
    /// Tiri answers differently on purpose. The entry is not going away, and the day it
    /// stops diverging is a day to check that tiri did not drift rather than a day to
    /// delete a line.
    Deliberate,
}

/// An entry silences one fixture from the step it names onwards — everything after a
/// divergence describes a state that already went wrong. The steps before it are still
/// compared, and other fixtures are untouched. Without this the choice would be between a
/// red suite and deleting the fixture that found the problem, and both of those end with
/// nobody recording anything.
pub(super) const KNOWN: &[Divergence] = &[
    Divergence {
        fixture: "open-after-swapping-into-a-tabbed.parity",
        verdict: Verdict::Open,
        step: 13,
        reason: "\
            A new view maps beside `seat_get_focus_inactive_view` of the workspace's most \
            recent tiling child (sway/tree/view.c:802-824), and after two swaps the two \
            compositors disagree about which view that is: sway answers the tab, tiri the \
            window inside the tab's sibling split. The map target is read the same way on \
            both sides, so what differs is the seat order a swap leaves behind, not the \
            insertion rule.",
    },
    Divergence {
        fixture: "swap-two-floating-roots.parity",
        verdict: Verdict::Open,
        step: 8,
        reason: "\
            Swapping a node with a top-level floating one: sway leaves `ws->floating` in the \
            order it found it, tiri raises the node that arrived. The stack order is what \
            the two disagree about, not the tree.",
    },
    Divergence {
        fixture: "swap-a-tab-with-a-window-behind-a-fullscreen.parity",
        verdict: Verdict::Deliberate,
        step: 8,
        reason: "\
            Reached through a swap: sway leaves the node the fullscreen arrange never visited \
            with the box it had, `0.334,0.000 0.334x1.000`, and tiri gives it 0x0. Only one \
            of the two boxes is empty, so the empty-box rule does not reach it; what is left \
            is the arrange itself. Tiri keeps arranging — see the entry below for why.",
    },
    Divergence {
        fixture: "move-left-under-a-fullscreen-sibling.parity",
        verdict: Verdict::Deliberate,
        step: 8,
        reason: "\
            sway's `arrange_workspace` hands the fullscreen node the output and returns, so \
            the branches it skipped keep whatever pending box they last had — 0x0 for one \
            built while something else was fullscreen. Tiri arranges them, to `1.000x1.000`. \
            \
            Not reproduced, on purpose. Nothing is drawn in either box: every node involved \
            is behind a fullscreen. What differs is a number an IPC client reads for a node \
            it cannot see, and matching it would mean carrying stale boxes through the tree \
            so that a query about something invisible returns sway's arithmetic. The three \
            differences the suite reports here are one: `erase_decoration` reseats a stacked \
            container's children onto its box, so the parent's 0x0 reaches both of them.",
    },
];

/// What makes two divergences "the same one".
///
/// The command that diverged, and where in the tree the differences landed. Not the values:
/// the same bug reached by a different route shows the same shape at the same places, with
/// whatever windows happened to be involved.
///
/// Two genuinely different bugs could collide here, and the cost of that is a real finding
/// reported as known. The other direction — a signature too precise to ever match — costs a
/// fuzz that only ever re-finds what it already knows, so this errs towards matching and
/// leans on the fixtures to keep each known entry honest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Signature {
    pub command: String,
    pub places: BTreeSet<String>,
}

impl Signature {
    pub fn of(command: &str, expected: &Workspace, actual: &Workspace) -> Self {
        Signature {
            command: command.to_owned(),
            places: expected
                .diff(actual)
                .into_iter()
                .map(|difference| difference.at)
                .collect(),
        }
    }
}

pub(super) fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tiri-parity/fixtures")
}

pub(super) fn read(path: &Path) -> Fixture {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read the fixture {name}: {err}"));
    Fixture::parse(&text).unwrap_or_else(|err| panic!("{name} is not a readable fixture: {err}"))
}

/// Compare a recording against tiri, decorations erased on both sides.
///
/// Erasing here rather than at record time is what lets the rule improve without a machine
/// with sway to regenerate the files.
pub(super) fn compare(recorded: &Workspace, replayed: &Workspace) -> (Workspace, Workspace) {
    let mut expected = recorded.clone();
    let mut actual = replayed.clone();
    erase_decoration(&mut expected);
    erase_decoration(&mut actual);
    (expected, actual)
}

/// The signature of every entry in the table, taken from the recordings themselves.
///
/// The fuzz needs to recognise these in scripts that were never written down, and deriving
/// them from the fixtures means the table stays the only place a divergence is described.
pub(super) fn signatures() -> Vec<Signature> {
    let mut out = Vec::new();
    for entry in KNOWN {
        let fixture = read(&fixtures_dir().join(entry.fixture));
        let replayed = replay(&fixture.script(), fixture.client);
        let Some(recorded_step) = fixture.steps.get(entry.step - 1) else {
            panic!(
                "{} has no step {}: the entry names a command that is not there",
                entry.fixture, entry.step
            );
        };
        let Some(replayed_step) = replayed.steps.get(entry.step - 1) else {
            panic!("{} replayed short of step {}", entry.fixture, entry.step);
        };

        let (expected, actual) = compare(&recorded_step.model, &replayed_step.model);
        let signature = Signature::of(&recorded_step.command, &expected, &actual);
        assert!(
            !signature.places.is_empty(),
            "{} step {} no longer diverges: {}",
            entry.fixture,
            entry.step,
            match entry.verdict {
                Verdict::Open => "delete the entry",
                Verdict::Deliberate =>
                    "tiri now matches sway where it had decided not to, which is a \
                     regression unless the decision changed",
            }
        );
        out.push(signature);
    }
    out
}
