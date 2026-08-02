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
A sway bug, not a tiri one — i3 gets this right. Size shares only; the tree matches. A window \
promoted out of a container keeps the percent it had inside that container, and the container \
it left has its own percent invalidated though it never moved. Building sway with those two \
swapped round produces tiri's 0.25/0.25/0.25/0.25 here and moves nothing in the other 27 \
fixtures. Listed, not fixed: the recording is of released sway.",
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
    // The three below are open findings from the differential fuzz, recorded but not yet
    // fixed. The first two are one cause: the workspace's layout lives outside the tree, in
    // `workspace_layout` / `pending_layout` / `workspace_prev_split_layout`, so every rule
    // phrased as "the parent is the workspace" has to re-derive it and they disagree. Fixing
    // them one at a time is what the collecting is meant to avoid.
    Divergence {
        fixture: "split-inside-a-tabbed-workspace.parity",
        step: 5,
        reason: "\
`split v` on the only window of a tabbed workspace. sway builds a splitv container inside the \
workspace and leaves the workspace tabbed; tiri overwrites the workspace's layout with splitv \
and builds nothing. With one window tiri's root is the leaf itself and `tabbed` is held \
outside the tree, so `split_focused` sees a window with no parent and takes the \
empty-workspace route.",
    },
    Divergence {
        fixture: "toggle-split-returns-to-the-previous-split.parity",
        step: 7,
        reason: "\
`layout toggle split` on a container that was made tabbed. sway returns it to the splith it \
had; tiri returns it to splitv. The container's own `prev_split_layout` is unset, and the \
fallback reaches for the *workspace's* — another node's memory of another command.",
    },
    Divergence {
        fixture: "toggle-split-on-a-workspace-of-windows.parity",
        step: 4,
        reason: "\
`layout toggle split` with the workspace selected and two windows in it. sway turns splith \
into splitv; tiri leaves it splith. The same memory as the entry above, read for the \
workspace itself rather than for a container.",
    },
    Divergence {
        fixture: "move-dissolves-containers-around-a-lone-window.parity",
        step: 5,
        reason: "\
A `move` by a window that is the only thing inside every container above it. sway dissolves \
them all and leaves the window alone on the workspace; tiri keeps tabbed holding splitv \
holding the window. tiri has the rule — `alone_all_the_way_up` — but reads it as \"do \
nothing\" where sway reads it as \"there is nothing left for these containers to hold\".",
    },
    Divergence {
        fixture: "move-dissolves-containers-and-turns-the-workspace.parity",
        step: 6,
        reason: "\
The same as above, reached through a `close`, and it also turns the workspace: sway ends \
splitv, tiri splith. Recorded separately because it pins both halves — the containers going \
and the workspace facing the move — where the other fixture only shows the first.",
    },
    Divergence {
        fixture: "toggle-split-on-a-mixed-workspace.parity",
        step: 5,
        reason: "\
`layout toggle split` on a workspace holding a container and a window. sway goes to splith, \
tiri to splitv. Third shape of the same memory, kept because it is the one where the \
workspace has children of both kinds.",
    },
    // The two below are the same question answered in both directions, which is why neither
    // is a rule about dissolving containers: sway drops the split in one and keeps a whole
    // nesting in the other, and tiri gets each one backwards.
    Divergence {
        fixture: "move-by-a-window-alone-in-a-stacked.parity",
        step: 4,
        reason: "\
`move up` by a window alone inside the splitv it was just given, inside a stacked container. \
sway drops the splitv; tiri keeps it.",
    },
    Divergence {
        fixture: "move-that-keeps-the-containers.parity",
        step: 6,
        reason: "\
`move down` out of a stacked container. sway keeps the nesting the window came from — \
splith holding stacked holding splith holding the window — and tiri flattens it to a splith \
holding the window. The mirror of the entry above.",
    },
    // Where a new window lands.
    Divergence {
        fixture: "open-with-a-container-selected.parity",
        step: 7,
        reason: "\
Opening a window while a container is selected. Both put it on the workspace and both agree \
on the sizes; they disagree on the slot — sway w2, w3, w1 against tiri w2, w1, w3. What is \
being asked is where `focus parent` leaves the insertion point, which nothing has measured \
yet.",
    },
    // Movement inside tabbed and stacked containers, where a direction says nothing about
    // where a window should land.
    Divergence {
        fixture: "move-a-tab-back-and-forth.parity",
        step: 6,
        reason: "\
Moving a tab out of a tabbed container and back leaves sway's tabs in their original order \
and tiri's reversed. Direction is meaningless inside a tabbed container, so where a \
returning tab lands is its own question.",
    },
    Divergence {
        fixture: "move-a-tab-up.parity",
        step: 7,
        reason: "\
`move up` by the focused tab. sway leaves the tab order alone; tiri swaps the moved tab with \
the one before it. The vertical case of the same question, and the pair says a move inside \
tabs does not reorder them on either axis.",
    },
    Divergence {
        fixture: "move-a-tab-sideways-when-nested.parity",
        step: 10,
        reason: "\
`move left` then `move right` by a tab in a tabbed container nested in a splitv. sway ends \
with the tab first, tiri second — the pair of moves is not the identity in either, and they \
disagree about where it lands.",
    },
    Divergence {
        fixture: "move-inside-nested-tabbed-and-stacked.parity",
        step: 8,
        reason: "\
`move right` by a tab inside a tabbed container that is itself inside a stacked one. sway \
keeps the window where it is; tiri promotes it out to the workspace. The same question as \
the entry above, asked while nested: a horizontal move has no meaning inside tabs, and tiri \
answers it by climbing until it finds an axis that does.",
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
