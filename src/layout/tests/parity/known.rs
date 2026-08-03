//! Divergences that are known, understood, and not yet closed.
//!
//! One table, read by two consumers that need different things from it. The fixture suite
//! needs to know *where* to stop comparing a recording; the fuzz needs to recognise a
//! difference it has already been told about, in a script nobody wrote down. Keeping the
//! table in one place is what stops those two from drifting into separate lists.
//!
//! Adding an entry is a claim that the difference is understood. Removing one is the point.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tiri_parity::{erase_decoration, Fixture, Workspace};

use super::replay;

pub(super) struct Divergence {
    pub fixture: &'static str,
    /// 1-based, counting the commands in the fixture.
    pub step: usize,
    pub reason: &'static str,
}

/// An entry silences one fixture from the step it names onwards — everything after a
/// divergence describes a state that already went wrong. The steps before it are still
/// compared, and other fixtures are untouched. Without this the choice would be between a
/// red suite and deleting the fixture that found the problem, and both of those end with
/// nobody recording anything.
pub(super) const KNOWN: &[Divergence] = &[
    Divergence {
        fixture: "move-into-a-stacked-inside-a-tabbed.parity",
        step: 9,
        reason: "\
Which tab the outer container shows. A window moved into a stacked container nested in a \
tabbed one is focused in both, but sway leaves the tabbed showing the sibling it was showing \
before, so the window it says is focused is one the user cannot see. Measured rather than \
guessed at: the state is still there two seconds later, and the first `focus` command of any \
kind heals it. `cmd_move`'s directional branch is the whole reason — it moves the node and \
never touches the seat, so the destination goes on showing whatever it was showing. \
Implementing that means implementing a compositor that hides the window it just focused, \
which is not a rule to port but a consequence of one not being applied. tiri raises the tab \
the window landed in and the divergence lasts exactly one command.",
    },
    // The four below are one mechanism, not four bugs, and the tree agrees in all of them:
    // only the size shares differ. sway invalidates a fraction whenever `move` disturbs it —
    // the container it moved, the ancestor it emptied — and `arrange` later fills an invalid
    // one with the average of the siblings that still have one, then normalizes. Three of the
    // four therefore come out even in sway and lopsided in tiri, which keeps the share the
    // disturbed slot was holding and divides it. The fourth comes out the other way round,
    // and is where the rule is a sway bug rather than a rule: i3 invalidates the container
    // that moved, sway invalidates the one that stayed.
    //
    // Modelling it means a fraction that can be unset, which tiri has no notion of. Until it
    // does, all four stay recorded: patching the arithmetic at each of these sites is exactly
    // the whack-a-mole this suite exists to stop.
    Divergence {
        fixture: "nested-same-orientation-after-a-move.parity",
        step: 8,
        reason: "\
The sway bug, and the one of the four where tiri's answer is i3's. A window promoted out of a \
container keeps the percent it had inside that container, and the container it left has its \
own percent invalidated though it never moved. sway 0.214/0.214/0.25/0.322, tiri even. \
Building sway with those two swapped round produces tiri's answer and moves nothing in the \
other fixtures.",
    },
    Divergence {
        fixture: "cross-the-workspace-leaving-one-container.parity",
        step: 7,
        reason: "\
sway a third each, tiri 0.5/0.25/0.25. Kept because it is what pinned the wrapping rule down \
— the sibling case, one fixture over, wraps instead of splicing and passes.",
    },
    Divergence {
        fixture: "move-up-then-right.parity",
        step: 5,
        reason: "\
i3 #145. sway a third each, tiri 0.25/0.25/0.5. The shortest script that reaches the splice \
at all.",
    },
    Divergence {
        fixture: "move-up-after-focus-child.parity",
        step: 8,
        reason: "\
sway a quarter each, tiri 0.5/0.167/0.167/0.167. Found by the fuzz; the deepest of the four, \
and the one that shows the share the squashed pair was holding surviving into a workspace \
that sway has levelled.",
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
        let replayed = replay(&fixture.script());
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
            "{} step {} no longer diverges; delete the entry",
            entry.fixture,
            entry.step
        );
        out.push(signature);
    }
    out
}
