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
        fixture: "a-container-around-a-fullscreen-window.parity",
        step: 5,
        reason: "\
A container built around a window that is already fullscreen has no box at all in sway: \
0x0, while the window inside it covers the output. It lasts exactly as long as the \
fullscreen does — the step after, leaving fullscreen gives the container the workspace and \
both agree again. sway arranges a workspace with a fullscreen container by arranging the \
fullscreen node against the output and never descending the tiled tree underneath, so a \
container created while that is true is simply never given a box. Nothing reads it while it \
is 0x0, which is why it survives in sway. tiri lays the container out whatever is fullscreen \
above it and reports the workspace, which is where the container will be the moment it \
matters. Recorded so the search moves past it; copying it would mean publishing a rectangle \
that describes nothing.",
    },
    Divergence {
        fixture: "floating-the-workspace.parity",
        step: 3,
        reason: "\
`floating toggle` with the workspace selected. sway's `cmd_floating` has no container to act \
on, so `workspace_wrap_children` builds one, the workspace goes splith, and the wrapper is \
what gets focused and floated — a container, even around a single window. The geometry now \
agrees exactly; what is left is the wrapper itself and the focus that sits on it.

That last piece is not in the layout, it is in what the two publish. tiri gives *every* \
floating group a container root and sway only has one when the group really is a container, \
so the tiri normalizer unwraps a lone floating group to keep the ordinary case comparable — \
and unwraps this one with it, where the container is real and addressable. Closing it means \
tiri's IPC saying which of the two it has, not the normalizer guessing from the child count.",
    },
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
    Divergence {
        fixture: "nested-same-orientation-after-a-move.parity",
        step: 8,
        reason: "\
One line of sway, and the only size share left in the corpus. Every reparenting site in \
`cmd_move` invalidates the fraction of the container it just moved — six of them — and the \
seventh, promoting a node to sit beside an ancestor, invalidates *the ancestor's* instead and \
keeps the moved node's, which is the two the wrong way round. i3 does what the other six do, \
and so does `reparent` here. sway 0.214/0.214/0.25/0.322, tiri even. Building sway with them \
swapped produces tiri's answer and moves nothing else in the corpus. The rest of the rule is \
implemented, which is what closed the three divergences that used to sit beside this one: an \
unset fraction, and the resolve that fills it with the average of the siblings that kept \
theirs.",
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
