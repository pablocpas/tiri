# i3/sway parity

Tiri treats sway as the behavioral reference for tiling commands. The goal is observable
compatibility: given the same command sequence, both compositors should expose the same tree,
focus, ordering, layout and normalized geometry. Their internal data structures do not need to
match.

## Test architecture

Parity has three layers:

1. `tiri-parity/fixtures/*.parity` contains command scripts and the model recorded from sway after
   every command.
2. `tiri-parity` parses those recordings and provides the compositor-independent observable model.
3. `src/layout/tests/parity/` replays every script against Tiri and compares each intermediate
   state. It also contains differential fuzzing for combinations that are not yet fixtures.

A fixture is both input and expected output. Lines beginning with `$ ` are commands in i3's
grammar; the blocks below them are sway's recorded states. Each file stamps the sway version and
the client size used for the recording, so reference upgrades produce a reviewable Git diff.
Normal test runs never need sway installed.

```text
                 record occasionally                 replay on every test run
i3 command script --------------------> fixture ------------------------------> Tiri
                      sway                           compare after each command
```

## What is compared

The model keeps behavior a user or IPC client can observe:

- tiled and floating hierarchy, sibling order and layout mode;
- focused window and selected container;
- fullscreen, marks and floating state;
- normalized geometry and size proportions.

Theme-dependent tab and stack decoration is erased before comparison. sway and Tiri reserve
different title-bar heights, which is not a layout-semantic difference. The subtree shape, tab
order, active child and container bounds remain compared. Normalization rules belong in
`tiri-parity`; compositor-specific behavior belongs in the replay adapter, not in fixtures.

A box with no area is compared by its emptiness alone. Neither compositor draws anything inside
one, and the numbers left in it are whatever the last pass wrote: an arrange that returns early
at a fullscreen node leaves the branches it skipped holding a stale box, or a negative one. Only
when both sides agree there is nothing — one having an area and the other not is a real
difference and is still reported.

Note what this means for anything that changes the working area itself, such as `struts`: both
adapters normalize geometry as `(rect - area) / area`, so a change to that area cancels on both
sides and cannot be seen here at all. Those belong in the layout unit tests, not in a fixture.

## When a difference is not debt

`src/layout/tests/parity/known.rs` records every difference that is understood but not gone.
Each entry carries a verdict, because "we understand this" and "we intend to fix this" are two
different claims:

- `Open` — Tiri should match sway here and does not yet. Deleting the entry is the goal, and
  the suite says so the moment it stops diverging.
- `Deliberate` — Tiri answers differently on purpose. The suite still checks it keeps
  diverging; the day it stops is a day to ask whether Tiri drifted, not a line to delete.

Reaching for `Deliberate` because a difference is inconvenient is how a parity suite stops
meaning anything. It is for a difference traced to something sway does that Tiri declines to
reproduce, with the reason written down — today, the boxes sway leaves on branches its arrange
returned early on, which describe nodes behind a fullscreen that neither compositor draws.

## Running parity tests

Run the fixture replay and harness tests with:

```sh
cargo test --lib layout::tests::parity
cargo test -p tiri-parity
```

For a targeted layout change, also run the closest layout test filter, for example
`cargo test -q move_`. A passing unit test is not a substitute for a fixture when the exact sway
behavior was uncertain.

## Recording a case from sway

Create a `.parity` file containing one command per line, then record it:

```sh
cargo run -p tiri-parity --bin record -- tiri-parity/fixtures/example.parity
```

The recorder needs sway, `swaymsg` and a Wayland test client. Override their paths with
`TIRI_PARITY_SWAY`, `TIRI_PARITY_SWAYMSG` and `TIRI_PARITY_CLIENT`. Re-record the complete corpus
only when intentionally changing the reference sway build; review the resulting fixture diff
before accepting it.

## Which sway

The newest release, built from its tag by `tiri-parity/oracle.sh` under `~/.cache/tiri-parity`.
Not a development tree, because a difference from unreleased code is not yet a difference from
sway; and not whatever the distribution ships, because that is a version behind and the two do
not agree. Between 1.11 and 1.12, sway changed how a tiled resize spreads across siblings and
how many container levels a `layout` command flattens — measuring against 1.11 would report
both of those upstream changes as divergences in tiri.

Every fixture stamps the version that answered it, so changing the reference is a reviewable
diff rather than a silent one. Build a different one with `TIRI_PARITY_SWAY_REF=<tag>` and point
the recorder or a campaign at it with `TIRI_PARITY_SWAY`: that is how to ask whether a
divergence is already fixed upstream, or which release changed a behaviour.

A tree without git history stamps itself `-dev` whatever it contains, which once made a release
build look like an unreleased one. `oracle.sh` clones the tag for exactly that reason.

## Differential fuzzing

With the sway oracle configured, run a single fuzz job with:

```sh
RUN_PARITY_FUZZ=1 cargo test --lib differential_fuzz_against_sway -- --nocapture
```

For a parallel campaign use `tiri-parity/campaign.sh [seeds] [seconds] [jobs]`. Optional domain
limits are documented at the top of that script. A reported divergence should be shrunk into a
small checked-in fixture before changing layout code; this keeps fixes reproducible and avoids
optimizing for one random seed.

## Known divergences

Intentional or unresolved differences live only in `src/layout/tests/parity/known.rs`. Each entry
names a fixture and first divergent step, explains why it is accepted temporarily, and is checked
against the current failure signature. Do not weaken the global normalizer or delete a fixture to
hide one divergence.

When behavior changes, the preferred workflow is:

1. capture the sway behavior in a minimal fixture;
2. add or adjust a focused Rust test;
3. implement the smallest layout change;
4. run the fixture suite and the relevant layout filters;
5. update `known.rs` only when a documented divergence must remain.
