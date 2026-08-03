//! Search for divergences instead of guessing at them.
//!
//! ```text
//! RUN_PARITY_FUZZ=1 cargo test --lib parity::fuzz -- --nocapture
//! ```
//!
//! Needs sway, so it never runs in CI; what CI consumes is the fixtures this produces. The
//! checked-in scenarios are all cases someone thought to write down, and every finding so
//! far came out of a combination someone happened to try. The space is (shape of the tree) ×
//! (what is selected) × (command), and it is not enumerable by hand.
//!
//! On a divergence the script is shrunk before it is reported, because "seed 2 diverged at
//! step 42" is the failure mode this whole effort exists to avoid. What comes out is the
//! shortest script that still shows the difference, ready to be saved as a fixture.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use tiri_parity::session::Sway;

/// Stops the session whatever happens to the test.
///
/// The comparison replays scripts through tiri, and a panic there — a broken invariant, say,
/// which is exactly what this is meant to find — would otherwise leave a headless sway
/// running for every aborted run.
struct Session(Sway);

impl std::ops::Deref for Session {
    type Target = Sway;
    fn deref(&self) -> &Sway {
        &self.0
    }
}

impl std::ops::DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Sway {
        &mut self.0
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.0.stop();
    }
}
use tiri_parity::Workspace;

use super::known::{self, Signature};
use super::replay;

/// Commands the generator draws from, with the weight each is drawn at.
///
/// Weighted rather than uniform: `open` builds the tree everything else needs, and
/// `focus parent` is the only way to reach a state where a container is what commands are
/// aimed at — which is where most of the differences found so far have lived.
///
/// Everything `script.rs` can parse belongs here. `floating toggle` and `fullscreen toggle`
/// were parseable and unlisted, which meant the search had never once interleaved them with
/// the tiling commands — the region a hand-written fixture is least likely to reach, since
/// nobody writes down a script whose point is that two unrelated commands happened in one
/// order rather than the other. They are drawn rarely: both take a window out of the tiling
/// for a while, and a script spent outside the tree is a script comparing very little.
const VOCABULARY: &[(&str, u32)] = &[
    ("open", 10),
    ("close", 2),
    ("split h", 4),
    ("split v", 4),
    ("layout splith", 3),
    ("layout splitv", 3),
    ("layout tabbed", 3),
    ("layout stacking", 3),
    ("layout toggle split", 2),
    ("layout toggle all", 2),
    ("split toggle", 2),
    ("floating toggle", 2),
    ("fullscreen toggle", 2),
    ("focus left", 3),
    ("focus right", 3),
    ("focus up", 3),
    ("focus down", 3),
    ("focus parent", 6),
    ("focus child", 3),
    ("move left", 4),
    ("move right", 4),
    ("move up", 4),
    ("move down", 4),
];

/// Windows are what make a script slow — each one is a client sway has to start — so scripts
/// stay small enough that a shrink is a handful of seconds rather than a coffee break.
const MAX_WINDOWS: usize = 5;
const SCRIPT_LEN: usize = 14;

/// A tiny deterministic generator, so a session is reproducible from its seed alone.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*: not a good random source, a perfectly good arbitrary one.
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u32) -> u32 {
        (self.next() % u64::from(bound.max(1))) as u32
    }
}

fn generate(rng: &mut Rng) -> Vec<String> {
    let total: u32 = VOCABULARY.iter().map(|(_, weight)| weight).sum();
    let mut script = vec!["open".to_owned()];
    let mut windows = 1usize;

    while script.len() < SCRIPT_LEN {
        let mut roll = rng.below(total);
        let command = VOCABULARY
            .iter()
            .find_map(|(command, weight)| match roll.checked_sub(*weight) {
                Some(rest) => {
                    roll = rest;
                    None
                }
                None => Some(*command),
            })
            .unwrap_or("open");

        match command {
            "open" if windows >= MAX_WINDOWS => continue,
            "open" => windows += 1,
            // `close` takes the whole focused subtree, so it can empty the workspace from
            // any count once a container is selected. Counting it as "all of them" keeps
            // the rest of the script testing something; the alternative is a script whose
            // tail runs on an empty workspace and compares nothing.
            "close" if windows <= 1 => continue,
            "close" => windows = 1,
            _ => {}
        }
        script.push(command.to_owned());
    }
    script
}

/// What the two compositors did with a script, up to the first step they disagreed on.
struct Comparison {
    step: usize,
    command: String,
    expected: Workspace,
    actual: Workspace,
    signature: Signature,
}

fn compare(sway: &mut Sway, script: &[String]) -> Result<Option<Comparison>, String> {
    sway.reset()?;
    let recorded = sway.run(script)?;

    let text: String = script.iter().fold(String::new(), |mut out, command| {
        let _ = writeln!(out, "{command}");
        out
    });
    let replayed = replay(&text);
    if replayed.steps.len() != recorded.len() {
        return Err(format!(
            "replayed {} commands but sway ran {}",
            replayed.steps.len(),
            recorded.len()
        ));
    }

    for (step, ((command, recorded), got)) in recorded.into_iter().zip(replayed.steps).enumerate() {
        let (expected, actual) = known::compare(&recorded, &got.model);
        let signature = Signature::of(&command, &expected, &actual);
        if !signature.places.is_empty() {
            return Ok(Some(Comparison {
                step,
                command,
                expected,
                actual,
                signature,
            }));
        }
    }
    Ok(None)
}

/// Drop commands while the divergence survives.
///
/// Deliberately naive — one pass, one command at a time, from the back. A cleverer search
/// would cost sway runs, and sway runs are the whole budget; this already turns fourteen
/// commands into three or four, which is the difference between a script someone can read
/// and one nobody will.
fn shrink(
    sway: &mut Sway,
    script: Vec<String>,
    signature: &Signature,
) -> Result<Vec<String>, String> {
    let mut best = script;
    let mut idx = best.len();
    while idx > 0 {
        idx -= 1;
        let mut candidate = best.clone();
        candidate.remove(idx);
        if candidate.is_empty() {
            continue;
        }
        // The same divergence, not merely *a* divergence: without this a script whose
        // finding is at step twelve can shrink into four commands showing something else
        // entirely — usually one already in the ledger — and the real finding is lost with
        // no sign that it ever existed.
        if compare(sway, &candidate)?.is_some_and(|found| found.signature == *signature) {
            best = candidate;
        }
    }
    Ok(best)
}

#[test]
fn differential_fuzz_against_sway() {
    if std::env::var_os("RUN_PARITY_FUZZ").is_none() {
        eprintln!("set RUN_PARITY_FUZZ=1 to run this; it needs sway on the machine");
        return;
    }

    let seed = std::env::var("PARITY_FUZZ_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(0x5EED);
    let budget = std::env::var("PARITY_FUZZ_SECONDS")
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(120));

    // Derived from the recordings, so the ledger stays the only place a divergence is
    // described. Without this the search stops at the first thing it re-finds, and since a
    // known one is usually reachable in a handful of commands, that is all it ever does.
    let known = known::signatures();

    let mut rng = Rng(seed);
    let mut sway = Session(Sway::start().expect("cannot start sway"));
    let started = Instant::now();
    let mut scripts = 0usize;
    let mut skipped = 0usize;
    let mut found = None;

    while started.elapsed() < budget && found.is_none() {
        let script = generate(&mut rng);
        scripts += 1;
        match compare(&mut sway, &script) {
            Ok(None) => {}
            Ok(Some(divergence)) if known.contains(&divergence.signature) => skipped += 1,
            Ok(Some(divergence)) => found = Some((script, divergence.signature)),
            Err(err) => {
                panic!("the harness broke after {scripts} scripts: {err}");
            }
        }
    }

    let Some((script, signature)) = found else {
        eprintln!(
            "{scripts} scripts, seed {seed:#x}: no divergence \
             ({skipped} were already in the ledger)"
        );
        return;
    };

    let script = shrink(&mut sway, script, &signature).expect("shrinking failed");
    let report = compare(&mut sway, &script).expect("the shrunk script stopped running");

    let Some(divergence) = report else {
        panic!("the divergence vanished while shrinking; that is a bug in the harness");
    };

    let listing: String = script.iter().fold(String::new(), |mut out, command| {
        let _ = writeln!(out, "{command}");
        out
    });
    panic!(
        "divergence after {scripts} scripts (seed {seed:#x}), {skipped} already known, \
         shrunk to {} commands:\n\n\
         {listing}\n\
         step {} — after `{}`:\n--- sway ---\n{}--- tiri ---\n{}\n\
         Save that script as tiri-parity/fixtures/<name>.parity and record it.",
        script.len(),
        divergence.step + 1,
        divergence.command,
        divergence.expected.render(),
        divergence.actual.render(),
    );
}
