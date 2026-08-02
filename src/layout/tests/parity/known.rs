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
        fixture: "floating.parity",
        step: 3,
        reason: "\
The size a window gets when it starts floating. Measured with a client asking for 400x300: \
sway gives it 396x288 — its own size, centred. tiri gives every floating window 50% by 75% \
of the working area, whatever it asked for. That default is inherited from niri rather than \
broken here, and changing it is a visible product decision rather than a parity fix, so it \
is written down with the measurement for someone to decide. Everything else about the \
transfer matches: which tree the window sits in, what the tiled side does without it, and \
where it lands on the way back.",
    },
    Divergence {
        fixture: "cross-the-workspace-leaving-one-container.parity",
        step: 7,
        reason: "\
Two differences already listed below, meeting in one step. The *shape* matches: crossing the \
workspace when the only thing left behind is a single container splices that container's \
children into the workspace rather than wrapping them, and tiri does that. What differs is \
the order sway leaves them in (w1, w3, w2 — its reversing splice, which tiri deliberately \
does not copy) and the size shares (1/3 each against tiri's 0.5, 0.25, 0.25). Kept as a \
recording because it is what pinned the wrapping rule down: the sibling case, one fixture \
over, wraps instead and passes.",
    },
    Divergence {
        fixture: "nested-same-orientation-after-a-move.parity",
        step: 8,
        reason: "\
Size shares, not tree shape — the tree matches. tiri divides the row evenly when a window \
leaves a container and joins its parent; sway runs i3's `con_fix_percent`, where the window \
carries the percent it had inside the container it left, that container's own percent is \
invalidated, and anything unset takes the average of the rest before normalizing. Measured \
across three shapes and predicted to the pixel in two of them; the derivation is in \
docs/design/parity.md. Open because implementing it means changing how tiri assigns shares \
on insert and removal, not because anything is still unknown.",
    },
    Divergence {
        fixture: "move-into-a-different-layout.parity",
        step: 7,
        reason: "\
The reversing splice again, and nothing else: both flatten the nesting completely and put \
the same three windows in a row, sway as w3, w1, w2 and tiri as w2, w1, w3. Recorded while \
closing the wrap-versus-splice question, and it is the case that showed the two agree on \
the shape once the flatten splices rather than promotes.",
    },
    Divergence {
        fixture: "move-up-then-right.parity",
        step: 5,
        reason: "\
i3 #145, and two known differences meeting again: the order sway leaves the spliced children \
in (w2, w1 against tiri's w1, w2), which is its reversing loop and deliberately not copied, \
and the size shares. Recorded while measuring what `preserve_on_single` was approximating, \
and kept because it is the shortest script that reaches the splice at all.",
    },
    Divergence {
        fixture: "move-into-a-nested-container.parity",
        step: 7,
        reason: "\
Deliberate. Moving a window into its sibling container leaves that container as the only \
child of a split, and sway then splices it into the workspace — reversing the order of the \
windows inside it as it goes (w2, w3 come back as w3, w2, and stay that way). Reordering \
windows is not something a user asked for, and reproducing it would mean copying the loop \
that does it. tiri keeps the container and leaves the order alone. The single-child case, \
where there is no order to scramble, does match: see move-across-the-workspace.parity.",
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
