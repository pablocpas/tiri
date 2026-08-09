# Phase 10 — Formalized i3/sway parity

Internal design note. Not published to the wiki (mkdocs serves `docs/wiki` only).

## Why the previous attempt turned into whack-a-mole

Parity was chased before and collapsed into fixing one case while breaking another. Two
causes, both still visible in the tests that survived.

**Tests were fuzz seeds.** Seven `parity_seed*_stepNN_*` tests replay opaque 40-command
sequences and assert only the end state. When one breaks you learn that *something*
diverged, not *which command*, so you fix the symptom and the next seed lights up.

**They compared tree shape, not observable behaviour.** Shape encodes representation
choices that a compositor is free to make differently. Chasing it means reimplementing
sway's internals, and every fix moves the bump elsewhere.

That second cause is not a hypothesis. Converting 55 tests to the public API in phase 8
produced 53 byte-identical results and exactly 2 divergences, and both were the same
representation question: whether the first window on a freshly-split empty workspace gets
its own wrapper container.

**The rule the whole design follows:** compare what a user can perceive, and erase
everything else *by construction*. A difference that cannot be observed must not be able to
fail a test.

## Ground truth, measured

Run against sway 1.11 headless (`WLR_BACKENDS=headless`, `default_border none`,
`gaps inner 0`), driving `swaymsg` and dumping `get_tree`. These four observations are the
normalizer's specification — they are not recalled, they were measured.

**A. `split v` on an empty workspace creates no container.**
Split, then open two windows:

```
workspace splitv
  win   1920x540
  win   1920x540
```

The workspace node itself carries the orientation; the windows are its direct children.

**B. `split v` on a lone window also creates no container**, and the orientation outlives
the window:

```
1 window + split v      →  workspace splitv / win
after killing it        →  workspace splitv            (empty, orientation kept)
two new windows         →  workspace splitv / win, win
```

**C. `split v` on a window that has siblings creates a container immediately** — before any
new window arrives — and sway keeps it at one child:

```
A, B on a splith workspace   →  splith / win, win
focus left; split v          →  splith / [splitv / win], win
open C                       →  splith / [splitv / win, win], win
```

**D. Consequently the workspace is a container with an orientation, not a wrapper.** In
sway a workspace always exists and always has a layout. tiri models the same state as a
bare-leaf root plus `workspace_layout`, which the tree has owned since phase 6.

The equivalence in D is the single most important normalization rule, because it is where
the two representations differ while the behaviour does not.

### The workspace rules, measured in full

Building the replayer forced these to be pinned down, because several tests disagreed about
them and each disagreement had to be settled by measurement rather than by argument. All of
the following were run against sway 1.11 headless:

| command | target | sway builds a container? |
|---|---|---|
| `split X` | empty workspace | no — the workspace records the orientation |
| `split X` | a lone window | no — the workspace takes the orientation, and keeps it after the window closes |
| `split X` | a window with siblings | yes — around that window, kept at one child |
| `split X` | the workspace (after `focus parent`) | **always** — the workspace's children move under a wrapper keeping the old layout, even with one child, even when the orientation does not change |
| `layout X` | empty workspace | no — the workspace takes the layout |
| `layout X` | the workspace (after `focus parent`) | no — the workspace takes the layout and its children reflow in place |
| `layout X` | a window whose parent is a real container | no — that container takes the layout |
| `layout X` | a window whose parent is the workspace | yes — a container with layout X takes all the workspace's children, unless X is already the workspace's layout, which does nothing |
| `layout X` | a window, after `focus parent` then opening a window | as a plain window: opening a window ends the elevation |

Two consequences worth stating, because both contradicted tests that claimed to encode sway:

- `split v` and `layout splitv` are **not** the same command on a workspace. One always
  wraps, the other never does.
- repeating a layout does not nest. `layout splith; layout splith` on a single-child split
  leaves it exactly as it was, and so does `layout splitv` — the asymmetry a comment in
  `split.rs` claimed (splitv nests one more level) is not sway's behaviour.
- nesting that *does* happen is real. `layout tabbed; split v; layout tabbed` builds
  `splith > tabbed > tabbed > win` in sway too, so the wrappers tiri used to collapse as
  "redundant" were a level sway keeps.

### Moving across the workspace, measured

`move <direction>` where the direction crosses the workspace's own orientation:

```
splith / win 1, win 2      move down on 2  →  splitv / [splith / win 1], win 2
```

The workspace takes the direction's orientation, everything else moves under one container
that keeps the old one, and the moved window becomes that container's sibling — before it
going up or left, after it going down or right. Nothing wraps the window being moved.

The wrapper is kept while it says something: holding one window *across* the workspace's
orientation is a real arrangement. It goes when it only re-states what its grandparent
already says, which is i3's `tree_flatten` — a container with one child, that child a split
whose layout matches the grandparent's. Moving the window back out therefore leaves the
workspace flat again, which is what sway does.

### Where the workspace level lives

tiri's root container **is** the workspace, which is why the normalizer maps one onto the
other. Everything above follows from taking that seriously: a command aimed at a window can
never change the root container's layout, because in sway that command cannot change the
workspace's layout either.

Three separate layers each had their own theory of this and each has been pointed at the
same rule:

- the tree, in `set_focused_layout_with_policy`;
- the tiling space's routing, which had a five-variant `WorkspaceLayoutTargetKind` deciding
  between four shapes;
- the workspace's command routing, which read a stored `workspace_focus` elevation that
  nothing cleared when a window opened. It is now derived from the tree's selection, and
  the stored flag is consulted only for the one state the tree cannot express — a workspace
  whose single child is a window.

### What the model can see

The comparison can only report a difference the model can express, so the model's resolution
is the ceiling on any amount of searching. Two things were raised to that ceiling after
being found blind:

- **Which node holds focus, not just which window.** A focused container was originally
  recorded as "no window focused", which made every such state look alike — `focus parent`
  once and twice were indistinguishable, though they send the next command somewhere else,
  and that is where most of the findings so far have come from. Focus is now a position:
  `focus=3` for a window, `focus=@0/1` for a container, `focus=@` for the workspace itself.
  Both compositors already published it; the normalizers were dropping it.

Still outside what it compares, each for a stated reason:

- the size share of each child as a number, rather than as the rectangles it produces;
- anything a single workspace on a single output cannot reach.

### What `preserve_on_single` is, and why it is wrong

tiri marks some containers "do not dissolve me even with one child". sway has no such flag,
and the family of divergences around single-child containers has been closed three times by
adjusting *when* the flag is set rather than by replacing it. An attempt to remove it
established the following, and stopped short of landing:

- The flag carries **two unrelated meanings**: "cleanup must not dissolve me", and "I am a
  container a user can address" (`focus parent` stops here, floating wrappers select here).
  Only the first is the problem; the second is closer to simply *true* for every container.
- Whether a lone container is redundant is **not a property of the container**. Measured:
  `split h` builds one holding a single window and it survives; a `close` that empties a
  container down to one child leaves it alone; a move elsewhere in the tree leaves it alone;
  and a container with the same orientation as its parent survives both a `split` and a move
  that reorders inside it.
- The rule is keyed on *what just happened*, not on a flag. Whether a lone container
  survives depends on the command, not on the container, and the answer is that **only a
  directional move normalizes nesting**. Every other command leaves what it built standing.

### Where the replacement stands

Done. `preserve_on_single` no longer exists; what is left is `is_user_container`, answering
only the question about addressability.

The rule itself is now sway's, ported rather than approximated. `layout X` and `move` are
the two commands that restructure, and both are transcriptions of the sway function that
does it:

- `move` is `container_move_in_direction`, with the two normalizations `cmd_move` runs
  around it — `container_reap_empty` on the parent the node left, and `workspace_squash`
  over the whole workspace. `workspace_squash` is where a redundant pair of crossing splits
  disappears, and it runs *only* here, which is the whole of "sometimes a lone container
  survives a move and sometimes it does not".
- `layout X` is `cmd_layout`: it operates on the focused node's **parent**, and when that
  parent is the workspace it does not hand the workspace a layout — it wraps the
  workspace's children in a new container instead, unless the workspace itself is what was
  selected.

Eight recorded divergences closed at once when those replaced the approximations, and they
had looked like eight unrelated bugs precisely because each hand-written rule was a partial
transcription of a different part of the same two functions. The lesson is worth keeping:
when several divergences in one command family resist individually, the cause is usually
that the family is not being implemented, only imitated.

Two attempts were reverted before this one landed. The second failed for a reason worth
recording, because it was not the rule: splicing moves every remaining leaf up a level, and
the cached layout keeps a *path* beside each leaf's geometry. A path is an address, not
geometry, and a mutation that reports no change — of which there turned out to be several —
skips the relayout that refreshes it. The fix is to stop trusting every path through the
tree to say what it did: `mutate_tree` now re-derives those addresses whatever the mutation
claimed, which is cheaper than the alternative and made the invariant hold by construction.

The trap to avoid is visible in the first attempt: two changes that looked like progress
each broke behaviour that had already been measured, because `wrap_workspace_children` serves
both `split` and `layout` and they disagree about the one-child case. Any replacement has to
be validated against the whole fixture set, not against the divergence in hand — which is
also why the rule above was worth measuring for its own sake before touching anything.

### The size a window gets when it starts floating, measured

sway's `floating_natural_resize`: the window gets `view->natural_width/height`, which
`handle_map` reads once from the toplevel's geometry and never updates, clamped to the
floating min/max, and `container_floating_resize_and_center` centres it. No fraction of the
working area is involved anywhere. Measured with a client asking for 400x300 on a 1280x720
output: 396x288 at 442,216 — the client's own size, centred.

tiri gave every floating window 50% by 75% of the working area, inherited from niri. That is
now the sway rule, which needed a `natural_size` on `LayoutElement`: `size()` is whatever the
tiling last stretched the window to, and the whole point of the rule is that it survives the
window having been tiled at some other size in between.

The 50% by 75% turns out to be sway's too, for the other case.
`container_floating_set_default_size` writes it first and `floating_natural_resize` then
overwrites it *only for a view*, so a floating **container** — what `floating toggle` on a
selected workspace produces — keeps exactly those proportions. Two rules that look like one
number, which is why measuring the window case did not settle the container one.

It also pinned down a piece of harness skew the corpus had never exercised. A window's own
size is invisible while it is tiled, so the replayer's 100x200 test window and the recorder's
`foot` had disagreed harmlessly all along; the moment floating size became behaviour, the
difference started reading as a divergence. `session::CLIENT` is the one place that size is
now stated, and the replayer takes its windows from it.

### How sway redistributes size when a window changes container

The first measurements below were made against Sway 1.11 by asking for its `percent` values
directly rather than inferring them from rectangles. They exposed deferred fraction resolution,
but not the full mutation rule used by final Sway 1.12:

1. the window arriving carries **the percent it had inside the container it left** — not an
   equal share, and not the share that slot had in the destination;
2. the container it left has its own percent **invalidated**, so it is treated as unset;
3. `con_fix_percent` then gives every unset child the *average* of the children that do have
   a percent, and normalizes the lot to 1.

Step 3 is why the depleted container lands on exactly `1/n` every time — the average of the
others, divided by a total that now includes it, is precisely that. It looked like a special
case until the arithmetic came out.

Predicted against measured, on a 1280px workspace of `[w1, w2, splith[…]]` with one window
moving out to the right:

| container keeps | predicted px | measured px |
|---|---|---|
| 1 child | 274, 274, 320, 411 | 274, 274, 320, 412 |
| 2 children | 320, 320, 320, 320 | 320, 320, 320, 320 |
| 3 children | 351, 351, 320, 259 | 349, 349, 320, 262 |

Final Sway 1.12 has two deliberate tree-surgery primitives, not one policy with exceptions:

- **ordinary reparenting** unsets both fractions on the arriving node while preserving the
  destination siblings' raw fractions;
- **promotion beside an ancestor** preserves both fractions on the arriving node and unsets
  the ancestor slot it escaped from. When the workspace first changes orientation, the target
  is unset during that reorientation, so the later promotion correctly carries those zeros.

Tiri stores width and height explicitly, rather than an “active” vector whose meaning can be
changed by a mutable axis tag. That makes detach/restore and promotion carry the same semantic
values even if a container changes orientation while a subtree is away.

Both mutation primitives share the second half:

- **the fill.** `apply_horiz_layout`/`apply_vert_layout` give every unset child the average
  of the children that still have a fraction, then normalize the list to 1. That average is
  why a container emptied by a move lands on exactly `1/n`. It runs once, when sway arranges,
  after the move *and* the squash have finished — resolving as each of them goes answers with
  the siblings a half-finished tree happens to have.

`nested-same-orientation-after-a-move`, `promotion-invalidates-escaped-ancestor`, and
`promoting-after-cross-axis-moves-hits-sway-fraction-bug` pin the ordinary, direct-promotion,
and reoriented-workspace routes against the final Sway 1.12 tag.

This i3 side is measured too, not inferred only from its source. With `i3`, `i3-msg`, Xvfb
and `xmessage` installed, the opt-in test below runs the exact promotion on an isolated X11
display and asserts the four equal shares reported by released i3:

```sh
RUN_I3_PARITY=1 cargo test -p tiri-parity released_i3_invalidates_the_promoted_window_fraction
```

Landing the halves separately does not work and the corpus says so: carrying the squash's
raw fractions without the invalidation moves `move-across-the-workspace`, `move-at-an-edge`
and `wrapper-left-by-a-move`, all three of which agree before and after.

## Known divergences

Differences that are real, understood, and not yet fixed. These live in code, as the `KNOWN`
table in `src/layout/tests/parity/known.rs`, so an entry has to name the fixture and step
it silences and say why — and the suite fails if an entry stops diverging, which is what
stops the list from rotting.

The two entries this section originally held, `layout X` on a workspace child and stale
cached geometry after a sibling leaves, were both closed by measurement. What the recorder
found in their place is in the table.

## What already exists

- **tiri IPC**: `Request::LayoutTree` → `Response::LayoutTree`, whose `LayoutTreeNode`
  carries `layout`, `focused`, `visible`, `rect`, `percent`, `marks`, `is_floating`,
  `children`. Close to one-to-one with sway's `get_tree` nodes.
- **tiri actions**: `SplitHorizontal`, `SplitVertical`, `SetLayout{SplitH,SplitV,Tabbed,Stacked}`,
  `ToggleSplitLayout`, `ToggleLayoutAll`, `FocusParent`, `FocusChild`, the focus/move
  families, `CloseWindow`. The whole i3 command surface we need is already addressable.
- **Headless backend** plus `--headless`, `--headless-outputs`,
  `--headless-output-{width,height}`: tiri runs with virtual outputs and no GPU surface,
  driven entirely over IPC. `scripts/profile_tiri_autopilot.py` already uses this shape of
  harness for profiling.
- **Final Sway 1.12** (tag `8886939`) and `foot` on the development machine. The current
  fixture corpus was re-recorded against that exact build.
- A typed `Op` command vocabulary and 90 recorded parity scenarios, plus differential fuzzing
  that draws only from commands the Tiri replayer can parse.

The observable model and the Sway/i3 harness described below are now in place.

## The design

### 1. The observable model

One structure, produced from either compositor. It holds only what a user can perceive:

```
Workspace {
    layout:  Split(H|V) | Tabbed | Stacked      // the workspace's own orientation
    focused: Option<WindowRef>
    nodes:   [Node]
}

Node = Window { id, rect: FracRect, visible: bool, floating: bool, marks: [String] }
     | Container { layout, explicit: bool, rect: FracRect, nodes: [Node] }
```

`FracRect` holds x/y/w/h as fractions of the working area, not pixels.

Window identity is **open order**, assigned by the harness as it issues `open` commands.
Never pid, never Wayland id, never title.

### 2. Normalization rules

Derived from the measurements above, each with the reason it exists:

| Rule | Why |
|---|---|
| Drop sway's `root` and `output` nodes; drop `__i3_scratch`. | Not part of the workspace model under test. |
| Represent the workspace as `Workspace.layout` + its children, on both sides. tiri's bare-leaf root becomes a workspace with that leaf as its only node. | Observation D: same state, different representation. |
| **Keep** single-child containers that came from an explicit split. | Observation C: sway keeps them too. Erasing them would hide a real difference. |
| Collapse single-child containers that are *not* explicit splits. | Pure representation; neither compositor's users can see them. |
| Compare rects as fractions with a tolerance (2e-3), never pixels. | Gaps, borders and title bars are configured, not behaviour. |
| Compare `visible` for every window; under tabbed/stacked only the selected child is visible. | This is the observable consequence of a tabbed layout. |
| Ignore `percent` unless it is the thing under test. | Derivable from rects; comparing both double-counts. |

The normalizer is the load-bearing component, so it gets its own unit tests before anything
consumes it — fed hand-written sway and tiri trees, asserting they normalize equal.

### 3. Scripts in i3's command grammar

The contract shared by both systems is i3's own command language. Scripts are plain text,
one command per line, reviewable in a diff:

```
open            # spawn a client; the harness assigns it the next id
split v
open
focus left
layout tabbed
```

`open` and `close` are the only harness pseudo-commands; everything else is passed through
to `swaymsg` verbatim and mapped to an `Op` for tiri. The mapping is a small table
(`split v` → `Op::SplitVertical`, `layout tabbed` → `Op::SetLayoutTabbed`, …), and it is
the only place the two vocabularies meet.

### 4. Record against sway, replay in CI

```
        record (occasional, needs sway)          replay (every CI run)
script ─────────────────────────────► fixture ─────────────────────────► tiri
        headless sway + foot                     Op sequence + LayoutTree
        normalize after every command            normalize after every command
```

A fixture is the script plus the normalized model after each step, plus the sway version it
was recorded from. CI never needs sway. Recording is rerun when sway is upgraded, and the
diff of a re-record is itself informative: it tells you what changed in sway.

The harness must pin the config on both sides — no gaps, no borders, one output at a fixed
size — so that geometry differences mean something.

### 5. Compare per step, then minimize

Comparing after every command localizes a divergence to a single command rather than a
40-step sequence. This is the same principle that made per-op `verify_invariants` worth
adding in phase 8, and it is precisely what the seed tests lacked.

On failure, shrink: drop commands from the script while the divergence survives, then
promote the minimal script to a named test describing the **rule**, not the seed. This is
what turns "seed 2 step 42 broke" into "moving a window out of a tabbed container should
leave the container behind".

### 6. A divergence ledger

Intentional differences get an explicit entry: script, step, observed difference, and why
tiri chooses differently. An unlisted divergence fails the run; a listed one is reported and
does not. Without this, every sway release and every deliberate deviation re-breaks the
whole suite — the other half of the whack-a-mole.

## Plan

1. **Observable model and normalizer**, with its own unit tests. Nothing else can be
   trusted before this is. *(done)*
2. **Script format and tiri replayer**, driven through `Op`. *(done)* Validating it meant
   settling the workspace rules above by measurement, which changed behaviour in three
   layers and rewrote or removed the tests that disagreed.
3. **Recorder against headless sway**; record fixtures for scripts covering the same ground.
   Every mismatch there is a real finding about tiri, about sway, or about a test's belief.
   *(done)* `cargo run -p tiri-parity --bin record -- tiri-parity/fixtures/*.parity` needs
   sway on the machine; comparing the recordings does not, so CI runs it everywhere. A
   fixture is its own script — every `$ ` line is a command — so the input and the
   expectation cannot drift apart, and re-recording after a sway upgrade is a reviewable
   diff that says what changed in sway.

   Recordings are stored before decoration is erased, on purpose: what counts as decoration
   is a rule in `tiri-parity`, and baking it into the files would mean every improvement to
   that rule needs a machine with sway to regenerate them.
4. **Ledger** *(done, see above)*, and a **minimizer** *(done)*: on a divergence, drop
   commands from the script while it survives, so what lands in the table is the shortest
   script that still shows it.

   Both feed the **differential fuzz** in `src/layout/tests/parity/fuzz.rs`:

   ```text
   RUN_PARITY_FUZZ=1 cargo test --lib parity::fuzz -- --nocapture
   ```

   It generates scripts, runs each on a live sway and on tiri, compares after every command,
   and shrinks whatever diverges before reporting it. Checked-in fixtures are cases someone
   thought to write down; this searches instead, which matters because the space is (shape of
   the tree) × (what is selected) × (command) and every finding so far came out of a
   combination someone happened to try.

   One sway session serves every script — starting sway is the slow part, so scripts reset
   by moving to a fresh workspace. The generator is weighted rather than uniform, biased
   towards `open` (nothing else has anything to work on without it) and `focus parent` (the
   only way to reach a state where commands are aimed at a container). Shrinking is one
   naive pass, dropping one command at a time: sway runs are the whole budget, and it
   already turns fourteen commands into four or five.

   What it found in its first minutes is in the git history: a split that did nothing when
   the parent's layout had been set explicitly, a window escaping a container landing one
   position too far, a no-op move that dissolved a container anyway, and an escape that gives
   up at the first ancestor without room.
5. **Widen the scripts** to the areas still only covered by hand-written expectations:
   floating transport, scratchpad, marks, fullscreen.

The seven `parity_seed*` tests that used to be the pilot for step 5 are gone. They came out
of the whack-a-mole attempt, so their expectations were snapshots of whatever tiri did at
the time rather than anything measured — several of them turned out to encode the stale
workspace-context bug fixed here, and one asserted that opening a window preserves an
elevation sway drops. Re-freezing their numbers would have re-asserted those beliefs under
names claiming sway's authority. What was worth keeping about them was the *scenarios*, and
those belong to the recorder in step 3, where sway answers instead of a snapshot.

## Risks

- **A weak normalizer reproduces the original problem.** Erase too little and the mole game
  returns; too much and real divergences pass silently. Its own tests are what keep it
  honest, which is why they come first.
- **Configuration skew.** Gaps, borders and font metrics must match, or geometry must be
  compared relatively. Getting this wrong produces noise that looks like divergence.
- **Recording is a manual gate.** A fixture is only as good as the sway version it came
  from, so the version belongs in the fixture.
- **Source/version skew.** Reading Sway master while recording a release can silently mix two
  behaviours. The corpus and oracle are pinned to final Sway 1.12 (`8886939`); a source port
  remains a hypothesis until that binary agrees with it. The doubly-nested layout and
  promotion fixtures exist specifically because earlier source/release comparisons disagreed.
- **Client startup is timing-dependent.** The probes needed a sleep after `exec foot`. The
  recorder must wait on the window actually appearing in `get_tree`, not on a timer.

## Where the model still differs from sway's

Six places, found by making one of them right and watching what stopped working. They are
listed because the divergences the fuzz keeps finding map onto them one to one, and because
each is finite: this is a list that can be finished, not a tail.

1. **The seat's focus order** — *fixed*. sway keeps one list per seat over every node and
   reads three questions off it: which tab a switcher shows, which window a descent lands on,
   what gets focus when the focused window closes. Tiri kept one list per container plus two
   loose pointers, which is a projection, and projections lose the coupling. It is now one
   list behind a type with private fields, because the bug was never the rule — it was thirty
   places able to change focus and one of them remembering to update the order.

2. **Fractions live in the parent** — *fixed*. Every leaf and container now carries its own
   `width_fraction` and `height_fraction`. Child lists contain only identities, so detaching,
   swapping and moving across workspace stores cannot separate a node from its shares. The
   parent-indexed `AxisFractions` and the surgery needed to keep it parallel with `children`
   are gone.

3. **`child_total_width`/`child_total_height` are missing** — *fixed*. Arrange records the
   gap-adjusted denominator on every child, beside its fractions, and tiled resize snaps each
   raw fraction from the rounded pending span against that stored denominator before applying
   the delta. The `0.199/0.199/0.199/0.203` history therefore survives rather than being
   replaced with an even `0.200` reconstructed from the current sibling list.

4. **`user_created`** — a flag tiri has and sway does not, consulted in twenty-two places.
   Extra state means states sway cannot reach, and every one of them is a question sway never
   asks and tiri has to answer alone.

5. **The workspace is a node** — a deliberate simplification, and i3 agrees with it, but it
   is paid for in `RootPolicy` and its forty-three uses, and in the one divergence left in the
   ledger.

6. **Tiling and floating are two trees** — *fixed*. sway has one workspace holding two lists,
   and a container moves between them without ceasing to be itself. Tiri now does the same:
   `ContainerTree` owns the tiled root and the floating roots in one node store, while
   `FloatingSpace` only owns the groups' stacking and geometry. Crossing calls
   `float_subtree`/`unfloat_subtree`, so the leaf keeps its `NodeKey` and the seat's order keeps
   referring to the same node. The detached-subtree round trip that rebuilt every key has been
   removed from this path. `NodeKey` allocation is also independent of the workspace store:
   the key lives with a tile while sticky, scratchpad and interactive move hold it outside a
   tree, and detached containers carry their original key. Moving a window or whole subtree
   to another workspace or output therefore inserts the same identities, matching sway's
   `container_move_to_workspace` detaching and attaching the same `sway_container`.
