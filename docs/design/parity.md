# Phase 10 — Formalized i3/sway parity

Internal design note. Not published to the wiki.

## Why the previous attempt turned into whack-a-mole

Parity was chased before and collapsed into fixing one case while breaking another.
Two causes, both visible in the tests that survived:

**Tests were fuzz seeds.** Seven `parity_seed*_stepNN_*` tests replay opaque 40-command
sequences and assert only the end state. When one breaks you learn that *something*
diverged, not *which command* diverged, so you fix the symptom and the next seed lights up.

**They compared tree shape, not observable behaviour.** Shape includes representation
choices sway made that tiri has no reason to copy — sway always has a workspace container,
tiri's root can be a bare leaf. Chasing shape means reimplementing sway's internals, and
every fix moves the bump somewhere else.

That second cause is not hypothetical. Converting 55 tests to the public API produced 53
byte-identical results and exactly 2 divergences, and both were the same representation
question: whether the first window on a freshly-split empty workspace gets its own wrapper
container. Observably it does not matter — what matters is that the second window lands
below the first. The old tests were failing on the wrapper.

**Design rule that follows:** compare what a user can perceive, and normalize away
everything else *by construction*. A difference that cannot be observed must not be able to
fail a test.

## What already exists

- `tiri-ipc`: `Request::LayoutTree` → `Response::LayoutTree`, whose `LayoutTreeNode`
  carries `layout`, `focused`, `visible`, `rect`, `percent`, `marks`, `is_floating`,
  `children`. This is close to a one-to-one match with sway's `get_tree` nodes.
- Headless backend plus `--headless`, `--headless-outputs`,
  `--headless-output-{width,height}` flags: tiri runs with virtual outputs and no GPU
  surface, driven entirely over IPC.
- `scripts/profile_tiri_autopilot.py` already spawns tiri headless and drives it over IPC
  for profiling. Same shape of harness as the one below.
- sway 1.11 is installed on the development machine and runs headless under
  `WLR_BACKENDS=headless`.
- A complete typed command vocabulary (`Op`, 140 variants, all fuzzable), and 74 parity
  tests carrying the behaviour of 20 named i3 issues.

So the primitives are in place. What is missing is the model to compare and the harness
around it.

## The design

### 1. Observable model

A normalized projection both compositors map into. It holds only what a user or a client
can perceive:

- which window has focus;
- for each window: its rectangle, and whether it is visible or hidden behind a tab;
- the order of windows along each axis;
- floating vs tiled, and marks.

The normalizer is the load-bearing part. It must erase:

- **single-child containers that are not explicit splits** — pure representation;
- **the implicit workspace container** — sway always has one, tiri may not;
- **exact pixels.** Compare geometry as fractions of the working area with a tolerance, or
  compare relative ordering and proportions. Gaps, borders and title bars are configured
  differently and are not the thing under test.

Window identity is by open order, not by pid or wayland id.

### 2. Scripts in i3's command language

The shared contract between the two systems is i3's command grammar — `split v`,
`layout tabbed`, `focus parent`, `move left`, `kill` — plus an `open` pseudo-command for
spawning a test client. tiri maps it to `Op`; sway takes it directly. One vocabulary, two
backends. Scripts are plain text, one command per line, reviewable in a diff.

### 3. Record against sway, replay in CI

A recorder runs a script against headless sway, capturing the normalized model **after
every command**, and writes a fixture. CI never needs sway: the replayer runs the same
script against tiri and compares each step against the recorded fixture. Recording is a
separate, occasional job, rerun when sway is upgraded.

### 4. Compare per step, then minimize

Comparing after every command localizes a divergence to a single command instead of a
40-step sequence — the same principle that made per-op `verify_invariants` worth adding in
phase 8. On failure, shrink the script to the shortest prefix that still diverges, then
promote that to a named test describing the *rule*, not the seed.

### 5. A divergence ledger

Intentional differences get an explicit entry: the script, the step, the observed
difference, and why tiri does it differently. Without this, every sway release and every
deliberate deviation re-breaks the whole suite, which is the other half of the whack-a-mole.
An unlisted divergence fails; a listed one is reported and does not.

## Plan

1. Observable model and normalizer, with unit tests for the normalizer itself (it is the
   part that decides what counts as a difference, so it needs its own tests).
2. Script format and the tiri replayer, driven through `Op`. Verify against the existing
   74 parity tests re-expressed as scripts — they should pass without recording anything,
   since they already encode believed-correct behaviour.
3. Recorder against headless sway; record fixtures for those same scripts. Any mismatch
   here is a real finding about tiri, sway, or a test's belief.
4. Minimizer and the divergence ledger.
5. Pilot: convert the seven `parity_seed*` tests. They are the whack-a-mole survivors, so
   they are the honest test of whether the normalizer holds.

## Risks

- **Configuration skew.** Gaps, borders and font metrics must match between the two, or
  geometry comparison must be relative. Getting this wrong produces noise that looks like
  divergence.
- **Recording is a manual gate.** Fixtures are only as good as the sway version they came
  from; the ledger must record it.
- **A weak normalizer reproduces the original problem.** If it erases too little, the mole
  game returns; if too much, real divergences pass silently. Its own tests are what keep it
  honest, which is why they come first.
