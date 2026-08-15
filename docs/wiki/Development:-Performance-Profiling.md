# Performance Profiling

Keep performance comparisons reproducible: build both revisions with the same profile, run them
on the same machine and power mode, and repeat marginal results. Store captures under `target/`,
which is ignored by Git.

## Tracy build

Use on-demand Tracy so instrumentation is active only while a capture is attached:

```sh
cargo build --locked --release \
  --features profile-with-tracy-ondemand \
  --target-dir target/tracy
```

Use `profile-with-tracy` only when startup itself is under investigation.

## Headless workflow comparison

`profile_tiri_workflows.py` is the default regression workflow. It owns an isolated headless Tiri
instance, drives real Wayland clients, captures Tracy and writes a machine-readable report.

```sh
scripts/profile_tiri_workflows.py run \
  --tiri target/tracy/release/tiri \
  --output-dir target/perf-baseline

# Rebuild the candidate with the same command, then capture it.
scripts/profile_tiri_workflows.py run \
  --tiri target/tracy/release/tiri \
  --output-dir target/perf-candidate

scripts/profile_tiri_workflows.py compare \
  --baseline target/perf-baseline/workflows.json \
  --candidate target/perf-candidate/workflows.json
```

The built-in workflows cover opening and closing terminals, terminal idle/activity, a local
browser animation and a multi-output session. The browser uses a temporary profile and a local
page, so network and user-profile variance do not enter the result. Use `--workflow` to restrict a
confirmation run.

Headless results are suitable for comparing compositor CPU work, action latency, redraw rate and
self-invalidation loops. They do not measure physical scanout, input-device latency or production
GPU-driver behavior.

## Individual scenarios

Use `profile_tiri_scenario.py` when developing or validating one scenario against an already
running session:

```sh
python3 scripts/profile_tiri_scenario.py \
  --scenario scripts/perf_scenarios/open_close.json \
  --window-cmd "foot --app-id perf-test" \
  --output-dir target/perf-open-close
```

A scenario declares `initial_windows` and a list of IPC actions or `spawn_window` steps. Validate
new JSON without touching a session by adding `--validate-only`. Keep scenarios deterministic and
small enough that a regression can be tied to one operation.

## Physical DRM measurements

For presentation latency or process-wide efficiency, run the Tracy build as the compositor on a
quiet TTY and capture from a terminal inside that Tiri session:

```sh
scripts/profile_tiri_real_tracy.py \
  --scenario scripts/perf_scenarios/terminal_activity.json \
  --window-cmd scripts/perf_workloads/run_terminal_activity.sh \
  --expected-exe target/tracy/release/tiri \
  --output-dir target/drm-baseline
```

`--expected-exe` verifies through the IPC socket and `/proc` that the intended binary is running.
The report combines Tracy presentation markers with process CPU time, context switches and page
faults measured during the workload. Captures without DRM vblank samples are rejected.

After capturing the candidate under the same output mode and workload, compare the reports:

```sh
scripts/analyze_tiri_perceptual.py compare \
  --baseline target/drm-baseline/perceptual.json \
  --candidate target/drm-candidate/perceptual.json
```

This measures from compositor input or client commit to DRM presentation. It does not include
device-to-kernel delay or panel scanout; literal input-to-photon claims still require external
measurement.

## Interpreting results

- Compare total CPU time for work that may be skipped; a lower call count can raise the remaining
  calls' mean while still being an improvement.
- Compare per-call cost only when both revisions perform equivalent work.
- Treat p95 step latency only as meaningful with enough measured repetitions.
- Confirm small regressions with an A-B-A or B-A-B run and no other CPU-intensive workload.
- Use Tracy zones to locate work, but use process-wide counters to decide whether total compositor
  efficiency changed.
