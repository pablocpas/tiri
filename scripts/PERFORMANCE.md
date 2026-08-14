# Reproducible performance workflows

`profile_tiri_workflows.py` runs real Wayland clients under an isolated headless
tiri instance, records Tracy, and compares captures from the same machine.

## Covered workflows

| Workflow | Client and activity | Outputs |
| --- | --- | ---: |
| `open_close_foot` | Open four Foot terminals, focus them, then close them | 1 |
| `terminal_idle` | Keep a Foot terminal idle for six seconds | 1 |
| `terminal_activity` | Stream paced, coloured output in Foot and toggle fullscreen | 1 |
| `browser_activity` | Run a local animated page in Brave, scroll, draw canvas frames, and toggle fullscreen | 1 |
| `multi_output_foot` | Move windows, columns, workspaces, and focus across outputs | 3 |

The browser uses a new temporary profile and a local page, so network and user
profile variance do not enter the result. The terminal producer is paced with a
monotonic clock. IPC operations have a timeout, so a compositor deadlock fails
the run rather than waiting forever.

## Capture a baseline and candidate

Build both revisions with the same release profile and Tracy feature:

```sh
cargo build --locked --release --features profile-with-tracy-ondemand --target-dir target/tracy
```

Capture an accepted revision. `target/` is ignored by Git, so this survives a
`/tmp` cleanup without dirtying the worktree:

```sh
scripts/profile_tiri_workflows.py run \
  --tiri target/tracy/release/tiri \
  --output-dir target/perf-baseline
```

After rebuilding the candidate revision, capture it without running another
CPU-intensive job in parallel:

```sh
scripts/profile_tiri_workflows.py run \
  --tiri target/tracy/release/tiri \
  --output-dir target/perf-candidate
```

Compare the reports:

```sh
scripts/profile_tiri_workflows.py compare \
  --baseline target/perf-baseline/workflows.json \
  --candidate target/perf-candidate/workflows.json
```

The command returns non-zero when a threshold or correctness invariant fails,
so it can be used as a CI step. `--workflow` can restrict a confirmation run to
one or more suspect workflows.

## What the detector checks

- Total CPU time per run for refresh, protocol/IPC, layout, and rendering zones.
- Per-call cost only for redraw/render zones whose work remains comparable.
- Step p95 latency when both reports have at least five measured samples. The
  default six repetitions provide one warm-up and five measured runs.
- No configure or layout self-invalidation loops.
- No unexplained increase in global redraw rate.

Incrementally filtered refresh zones must be compared by total CPU time, not by
their conditional mean. After filtering, only expensive dirty calls remain, so
their mean can rise while total work falls sharply.

Use the same machine, power mode, output sizes, clients, and repeat count for
both reports. Confirm a marginal failure with an A-B-A or B-A-B rerun. Headless
captures exercise real client commits and compositor rendering, but they do not
measure physical scanout, presentation latency, real input-device latency, or a
GPU driver's performance on the production backend.

## Physical presentation latency

The `profile-with-tracy-ondemand` build also correlates visual work on the DRM backend and permits
the campaign runner to attach one Tracy capture per workload without restarting the compositor:

```text
physical input or IPC → client surface commit → redraw queue → render → KMS submit → DRM vblank
```

Run the Tracy build as the actual TTY session compositor. From a terminal inside
that session, capture a manual input task without restarting or stopping tiri:

```sh
scripts/profile_tiri_real_tracy.py \
  --duration 30 \
  --manual-task "type and scroll continuously in Foot" \
  --expected-exe target/tracy/release/tiri \
  --output-dir target/perceptual-terminal
```

`--expected-exe` connects to `TIRI_SOCKET`, reads the server PID through Linux
peer credentials, and rejects the run unless `/proc/PID/exe` has the expected
SHA-256. The PID, executable path and hash are retained in `metadata.json`.
This prevents accidentally labelling a capture made by the wrong compositor.

For client commits and compositor actions, an existing scenario can run on the
physical backend too:

```sh
scripts/profile_tiri_real_tracy.py \
  --scenario scripts/perf_scenarios/browser_activity.json \
  --window-cmd scripts/perf_workloads/run_browser_activity.sh \
  --expected-exe target/tracy/release/tiri \
  --output-dir target/perceptual-browser
```

The runner refuses captures that do not contain DRM vblank samples. Its
`perceptual.json` report contains p50/p95/p99 for input-to-presentation,
commit-to-presentation, queue/render/submit-to-presentation, latency in refresh
intervals, deadline misses, sources, and outputs.

Compare two captures made with the same output mode, workload and sample count:

```sh
scripts/analyze_tiri_perceptual.py compare \
  --baseline target/perceptual-before/perceptual.json \
  --candidate target/perceptual-after/perceptual.json
```

On fixed-refresh DRM outputs, Tracy also exports four adaptive-scheduler plots per output:

- `<output> frame schedule margin, ms`: total render budget before vblank.
- `<output> frame schedule delay, ms`: how long the queued redraw was coalesced before rendering.
- `<output> predicted render time, ms`: time-weighted render estimate plus positive deviation.
- `<output> frame schedule late penalty, ms`: extra budget learned from missed presentation targets.

After warm-up, the render estimate and margin should stabilize, `submit_ms` should move close to the
predicted vblank, and `late_ms`/missed-deadline rate must not regress. A late penalty that remains
near a whole refresh interval indicates repeated misses and fails the optimization even if median
input latency improved. The first frame after output initialization and frames after long idle are
deliberately rendered immediately and must not be treated as scheduler fallbacks.

This measures from the compositor receiving a libinput event to the DRM vblank.
It includes the application's response when the resulting surface commit arrives
within 750 ms. It does not include the device-to-kernel delay or panel scanout;
an LED/photodiode or high-speed-camera A/B test is still required for a literal
input-to-photon claim.

### P1 A-B-A protocol

The two prepared executables are recorded in
`target/perceptual-binaries/manifest.json`. The complete campaign is driven by
one Bash launcher. Switch from KDE to a spare text VT (for example,
`Ctrl+Alt+F3`), log in, and run:

```sh
cd /home/sergio/Documentos/tiri3/niri
scripts/profile_tiri_perceptual_ab.sh
```

The launcher refuses to run from Konsole, Tiri, SSH or a pseudo-terminal. It
starts the executables directly on the spare VT, without `--session`, so it
does not replace KDE's systemd graphical session. Before any workload, it also
requires two consecutive Tracy connections; this catches a non-on-demand binary
before spending time on the campaign. The order is:

1. `target/perceptual-binaries/before/tiri`
2. `target/perceptual-binaries/after/tiri`
3. `target/perceptual-binaries/before/tiri` again

For each compositor it first runs deterministic open/close, terminal and local
browser workloads without user interaction. It then asks for 120 seconds of
normal terminal, browser and window-management work. The deterministic half is
strictly comparable; the normal-work half is grouped by physical input source
and accepted only when `after` agrees against both surrounding `before` runs.
This separates a P1 effect from thermal, cache and human-input drift without
pretending that a person can repeat exactly the same gestures.

Each compositor closes automatically after its capture; use disposable work
with no unsaved documents. The launcher pauses on the text VT before starting
the next binary. The final numerical
comparisons are written to `comparison.txt`; the latest campaign path is written
to `target/perceptual-runs/LATEST`. Switch back to KDE only once the launcher
returns to the text console.
