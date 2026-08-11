//! Search for divergences instead of guessing at them.
//!
//! ```text
//! RUN_PARITY_FUZZ=1 cargo test --lib parity::fuzz -- --nocapture
//! RUN_PARITY_FUZZ=1 PARITY_FUZZ_TILING_ONLY=1 cargo test --lib parity::fuzz -- --nocapture
//! RUN_PARITY_FUZZ=1 PARITY_FUZZ_TILING_ONLY=1 PARITY_FUZZ_REFERENCE=i3 \
//!     cargo test --lib parity::fuzz -- --nocapture
//! ```
//!
//! Needs the selected reference compositor (`sway` by default), so it never runs in CI; what
//! CI consumes is the fixtures this produces. The checked-in scenarios are all cases someone
//! thought to write down, and every finding so far came out of a combination someone happened
//! to try. The space is (shape of the tree) × (what is selected) × (command), and it is not
//! enumerable by hand.
//!
//! On a divergence the script is shrunk before it is reported, because "seed 2 diverged at
//! step 42" is the failure mode this whole effort exists to avoid. What comes out is the
//! shortest script that still shows the difference, ready to be saved as a fixture.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use tiri_parity::session::{Sway, I3};

/// The selected reference. Each session owns its processes through RAII, including on panic.
enum Reference {
    Sway(Sway),
    I3(I3),
}

impl Reference {
    fn start(name: &str) -> Result<Self, String> {
        match name {
            "sway" => Sway::start().map(Reference::Sway),
            "i3" => I3::start().map(Reference::I3),
            other => Err(format!(
                "unknown PARITY_FUZZ_REFERENCE `{other}`; expected `sway` or `i3`"
            )),
        }
    }

    fn reset(&mut self) -> Result<(), String> {
        match self {
            Reference::Sway(session) => session.reset(),
            Reference::I3(session) => session.reset(),
        }
    }

    fn run(&mut self, script: &[String]) -> Result<Vec<(String, Workspace)>, String> {
        match self {
            Reference::Sway(session) => session.run(script),
            Reference::I3(session) => session.run(script),
        }
    }

    fn client_size(&self) -> (i32, i32) {
        match self {
            Reference::Sway(session) => session.client_size(),
            Reference::I3(session) => session.client_size(),
        }
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
    ("open", 12),
    ("close", 2),
    ("split h", 4),
    ("split horizontal", 1),
    ("splith", 1),
    ("split v", 4),
    ("split vertical", 1),
    ("splitv", 1),
    ("split toggle", 2),
    ("split t", 1),
    ("splitt", 1),
    ("split none", 1),
    ("split n", 1),
    ("layout splith", 3),
    ("layout splitv", 3),
    ("layout tabbed", 3),
    ("layout stacking", 3),
    ("layout default", 1),
    ("layout toggle", 1),
    ("layout toggle split", 2),
    ("layout toggle all", 2),
    ("layout toggle tabbed", 1),
    ("layout toggle splith tabbed stacking splitv", 1),
    ("floating toggle", 2),
    ("fullscreen toggle", 2),
    // Every resize grammar branch is represented. Amounts cover sub-rounding, ordinary,
    // large and floor-rejected changes; adding every amount would add no command semantics.
    ("resize grow width 1 px", 1),
    ("resize grow width 100 px", 2),
    ("resize shrink width 200 px", 2),
    ("resize grow height 100 px", 2),
    ("resize grow height 400 px", 1),
    ("resize shrink height 200 px", 2),
    ("resize grow left 150 px", 1),
    ("resize shrink left 100 px", 1),
    ("resize grow right 150 px", 1),
    ("resize shrink right 100 px", 1),
    ("resize grow up 150 px", 1),
    ("resize shrink up 100 px", 1),
    ("resize grow down 150 px", 1),
    ("resize shrink down 100 px", 1),
    ("resize set width 500 px", 1),
    ("resize set width 2000 px", 1),
    ("resize set height 400 px", 1),
    ("resize set height 2000 px", 1),
    ("focus next", 2),
    ("focus prev", 2),
    ("focus next sibling", 1),
    ("focus prev sibling", 1),
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

/// Windows are what make a script slow — each one is a client sway has to start — so these
/// are a budget, not a limit of the model.
///
/// They were 5 and 14, and 14 was too close to the thing being looked for: the divergence in
/// `resize-a-branch-inside-a-stacked` took eleven commands to reach and left a tree four
/// levels deep. A search whose scripts end three commands after its deepest known find is
/// measuring the budget as much as the layout, so both were raised until the shapes the fuzz
/// builds are past anything the corpus has needed. The cost is per-script time, which the
/// campaign spends anyway; the shrink is what gets slower, and it only runs once.
const MAX_WINDOWS: usize = 8;
const SCRIPT_LEN: usize = 28;

/// Sway 1.12 (tag 8886939) dereferences the destroyed workspace from `cmd_move_in_direction`
/// after this sequence. The backtrace ends in `arrange_workspace`; Tiri must remain alive.
/// Keep it in the search as an oracle failure instead of deleting valid commands from the
/// vocabulary.
const SWAY_1_12_MOVE_SELECTED_WORKSPACE_CRASH: &[&str] = &[
    "open",
    "layout toggle split",
    "focus parent",
    "focus parent",
    "split horizontal",
    "move up",
];

/// Two more paths to the same pinned-sway failure, reduced from deterministic campaign seeds.
/// In both, the second workspace-level split leaves `cmd_move_in_direction` arranging a
/// workspace it just destroyed. Keeping the exact scripts makes an unrelated disconnect on the
/// same move direction remain a harness failure.
const SWAY_1_12_MOVE_LEFT_SELECTED_WORKSPACE_CRASH: &[&str] = &[
    "open",
    "move up",
    "layout toggle split",
    "focus parent",
    "focus parent",
    "split v",
    "move left",
];

const SWAY_1_12_MOVE_RIGHT_SELECTED_WORKSPACE_CRASH: &[&str] = &[
    "open",
    "focus parent",
    "split toggle",
    "focus parent",
    "split toggle",
    "move right",
];

const SWAY_1_12_MOVE_LEFT_AFTER_REPEATED_SPLIT_TOGGLE_CRASH: &[&str] = &[
    "open",
    "focus parent",
    "split toggle",
    "focus parent",
    "split toggle",
    "move left",
];

/// Further minimal routes to the same `arrange_workspace <- cmd_move_in_direction` SIGSEGV,
/// found after the arena fixes let the campaign search past its earlier divergences. These
/// were verified against the pinned binary under gdb. They stay exact: an arbitrary sway
/// disconnect during the same final command is still a broken harness, not a known crash.
const SWAY_1_12_MOVE_UP_AFTER_TOGGLE_ALL_CRASH: &[&str] = &[
    "open",
    "layout toggle all",
    "focus parent",
    "focus parent",
    "split horizontal",
    "move up",
];

const SWAY_1_12_MOVE_DOWN_AFTER_TOGGLE_ALL_CRASH: &[&str] = &[
    "open",
    "layout toggle all",
    "focus parent",
    "focus parent",
    "split horizontal",
    "move down",
];

const SWAY_1_12_MOVE_RIGHT_AFTER_EXPLICIT_SPLITS_CRASH: &[&str] = &[
    "open",
    "focus parent",
    "split vertical",
    "focus parent",
    "split vertical",
    "move right",
];

const SWAY_1_12_MOVE_LEFT_NESTED_SWITCHERS_CRASH: &[&str] = &[
    "layout stacking",
    "open",
    "split horizontal",
    "open",
    "focus right",
    "split horizontal",
    "focus next sibling",
    "layout splitv",
    "move down",
    "focus prev sibling",
    "move left",
];

const SWAY_1_12_MOVE_DOWN_NESTED_TABS_CRASH: &[&str] = &[
    "open",
    "layout toggle all",
    "focus parent",
    "layout tabbed",
    "open",
    "move left",
    "focus parent",
    "open",
    "move left",
    "focus parent",
    "layout toggle all",
    "focus parent",
    "move down",
];

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

/// What a campaign is allowed to reach for.
///
/// Two exclusions, because they are two questions. Floating is a layer sway runs the tree
/// commands over and tiri reimplements beside the tree, so keeping it out asks "is the tree
/// itself right?". Fullscreen is not a layer at all — it is one branch of `arrange_workspace`
/// over the ordinary tiled tree — so excluding it does not narrow the domain, it hides part of
/// it. They used to be one flag, which made a tiling campaign look cleaner than the tiling
/// actually was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Domain {
    floating: bool,
    fullscreen: bool,
}

impl Domain {
    pub(super) const EVERYTHING: Self = Self {
        floating: true,
        fullscreen: true,
    };
    /// The tiled tree and every way of reshaping it, fullscreen included.
    pub(super) const NO_FLOATING: Self = Self {
        floating: false,
        fullscreen: true,
    };
    /// The tree commands alone. What i3 campaigns can be compared against.
    pub(super) const TREE_ONLY: Self = Self {
        floating: false,
        fullscreen: false,
    };

    fn allows(self, command: &str) -> bool {
        match command {
            "floating toggle" => self.floating,
            "fullscreen toggle" => self.fullscreen,
            _ => true,
        }
    }
}

fn generate(rng: &mut Rng, domain: Domain) -> Vec<String> {
    let allowed = |command: &str| domain.allows(command);
    let total: u32 = VOCABULARY
        .iter()
        .filter(|(command, _)| allowed(command))
        .map(|(_, weight)| weight)
        .sum();
    let mut script = vec!["open".to_owned()];
    let mut windows = 1usize;

    while script.len() < SCRIPT_LEN {
        let mut roll = rng.below(total);
        let command = VOCABULARY
            .iter()
            .filter(|(command, _)| allowed(command))
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

fn compare(reference: &mut Reference, script: &[String]) -> Result<Option<Comparison>, String> {
    reference.reset()?;
    let recorded = reference.run(script)?;

    let text: String = script.iter().fold(String::new(), |mut out, command| {
        let _ = writeln!(out, "{command}");
        out
    });
    // Whatever this session's client actually maps at, rather than an assumption.
    let replayed = replay(&text, reference.client_size());
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
    reference: &mut Reference,
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
        if compare(reference, &candidate)?.is_some_and(|found| found.signature == *signature) {
            best = candidate;
        }
    }
    Ok(best)
}

fn same_reference_failure(expected: &str, actual: &str) -> bool {
    disconnected_during(expected).is_some()
        && disconnected_during(expected) == disconnected_during(actual)
}

/// Identify a compositor disconnect by the command whose execution or observation failed.
/// Comparing only the transport message can shrink one crash into an unrelated crash.
fn disconnected_during(error: &str) -> Option<&str> {
    if !error.contains("Unable to connect") && !error.contains("Unable to receive IPC response") {
        return None;
    }
    ["while running `", "after `"]
        .into_iter()
        .find_map(|prefix| {
            let rest = error.split_once(prefix)?.1;
            rest.split_once('`').map(|(command, _)| command)
        })
}

/// Reduce a reference-compositor failure with a fresh process for every candidate.
///
/// A crashed compositor cannot be reset and reused like a layout divergence can. Starting a
/// new one is slower, but reference crashes are rare and a minimal reproducer is the only
/// useful output when they do happen.
fn shrink_reference_failure(name: &str, script: Vec<String>, error: &str) -> Vec<String> {
    let mut best = script;
    loop {
        let mut changed = false;
        let mut idx = best.len();
        while idx > 0 {
            idx -= 1;
            let mut candidate = best.clone();
            candidate.remove(idx);
            if candidate.is_empty() {
                continue;
            }

            let failed_alike = Reference::start(name)
                .and_then(|mut reference| {
                    reference.reset()?;
                    reference.run(&candidate).map(|_| String::new())
                })
                .is_err_and(|candidate_error| same_reference_failure(error, &candidate_error));
            if failed_alike {
                best = candidate;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    best
}

fn is_known_reference_failure(name: &str, script: &[String], error: &str) -> bool {
    if name != "sway" {
        return false;
    }

    let failure = disconnected_during(error);
    [
        ("move up", SWAY_1_12_MOVE_SELECTED_WORKSPACE_CRASH),
        ("move left", SWAY_1_12_MOVE_LEFT_SELECTED_WORKSPACE_CRASH),
        ("move right", SWAY_1_12_MOVE_RIGHT_SELECTED_WORKSPACE_CRASH),
        (
            "move left",
            SWAY_1_12_MOVE_LEFT_AFTER_REPEATED_SPLIT_TOGGLE_CRASH,
        ),
        ("move up", SWAY_1_12_MOVE_UP_AFTER_TOGGLE_ALL_CRASH),
        ("move down", SWAY_1_12_MOVE_DOWN_AFTER_TOGGLE_ALL_CRASH),
        (
            "move right",
            SWAY_1_12_MOVE_RIGHT_AFTER_EXPLICIT_SPLITS_CRASH,
        ),
        ("move left", SWAY_1_12_MOVE_LEFT_NESTED_SWITCHERS_CRASH),
        ("move down", SWAY_1_12_MOVE_DOWN_NESTED_TABS_CRASH),
    ]
    .into_iter()
    .any(|(command, expected)| {
        failure == Some(command)
            && script
                .iter()
                .map(|command| canonical_command(command))
                .eq(expected.iter().copied().map(canonical_command))
    })
}

fn canonical_command(command: &str) -> &str {
    match command {
        "split h" => "split horizontal",
        "split v" => "split vertical",
        other => other,
    }
}

#[test]
fn reference_crash_signature_includes_the_failing_command() {
    let move_up = "while running `move up`: Unable to connect to /tmp/sway.sock";
    let move_down = "while running `move down`: Unable to connect to /tmp/sway.sock";
    let move_right = "after `move right`: Unable to receive IPC response";
    assert!(same_reference_failure(move_up, move_up));
    assert!(!same_reference_failure(move_up, move_down));
    assert!(!same_reference_failure(move_up, "sway rejected `move up`"));
    assert_eq!(disconnected_during(move_right), Some("move right"));
}

#[test]
fn pinned_sway_workspace_move_crashes_are_known_by_exact_script() {
    for (command, script) in [
        ("move up", SWAY_1_12_MOVE_SELECTED_WORKSPACE_CRASH),
        ("move left", SWAY_1_12_MOVE_LEFT_SELECTED_WORKSPACE_CRASH),
        ("move right", SWAY_1_12_MOVE_RIGHT_SELECTED_WORKSPACE_CRASH),
        (
            "move left",
            SWAY_1_12_MOVE_LEFT_AFTER_REPEATED_SPLIT_TOGGLE_CRASH,
        ),
        ("move up", SWAY_1_12_MOVE_UP_AFTER_TOGGLE_ALL_CRASH),
        ("move down", SWAY_1_12_MOVE_DOWN_AFTER_TOGGLE_ALL_CRASH),
        (
            "move right",
            SWAY_1_12_MOVE_RIGHT_AFTER_EXPLICIT_SPLITS_CRASH,
        ),
        ("move left", SWAY_1_12_MOVE_LEFT_NESTED_SWITCHERS_CRASH),
        ("move down", SWAY_1_12_MOVE_DOWN_NESTED_TABS_CRASH),
    ] {
        let script: Vec<String> = script.iter().map(|line| (*line).to_owned()).collect();
        let error = format!("after `{command}`: Unable to connect to /tmp/sway.sock");
        assert!(is_known_reference_failure("sway", &script, &error));

        let mut different = script.clone();
        different.insert(0, "focus left".into());
        assert!(!is_known_reference_failure("sway", &different, &error));
    }
}

#[test]
fn a_restricted_generator_never_leaves_its_domain() {
    for seed in 1..=256 {
        let script = generate(&mut Rng(seed), Domain::TREE_ONLY);
        assert!(!script
            .iter()
            .any(|command| matches!(command.as_str(), "floating toggle" | "fullscreen toggle")));

        // The one the flags used to conflate: no floating, but fullscreen is still tiling and
        // stays in.
        let script = generate(&mut Rng(seed), Domain::NO_FLOATING);
        assert!(!script.iter().any(|command| command == "floating toggle"));
    }
}

#[test]
fn fuzz_vocabulary_is_unique_and_parseable() {
    let mut seen = std::collections::BTreeSet::new();
    for (command, weight) in VOCABULARY {
        assert!(*weight > 0, "`{command}` has no chance of being generated");
        assert!(seen.insert(*command), "duplicate fuzz command `{command}`");
        super::script::parse(command, tiri_parity::session::CLIENT)
            .unwrap_or_else(|err| panic!("fuzz command `{command}` is not replayable: {err}"));
    }
}

/// The search space a campaign ran in.
///
/// Printed with every result and compared before any two results are. A count of clean seeds
/// means nothing on its own: raising `SCRIPT_LEN` from 14 to 28 turned five clean seeds into
/// five divergent ones without a line of layout code changing, because the number was
/// describing the budget and not the compositor. So the budget travels with the number.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Space {
    domain: Domain,
    script_len: usize,
    max_windows: usize,
    vocabulary: usize,
    budget: Duration,
}

impl Space {
    fn current(domain: Domain, budget: Duration) -> Self {
        Self {
            domain,
            script_len: SCRIPT_LEN,
            max_windows: MAX_WINDOWS,
            vocabulary: VOCABULARY
                .iter()
                .filter(|(command, _)| domain.allows(command))
                .count(),
            budget,
        }
    }

    fn render(&self) -> String {
        format!(
            "domain {:?}, {} commands in the vocabulary, scripts of {}, up to {} windows, \
             {}s per seed",
            self.domain,
            self.vocabulary,
            self.script_len,
            self.max_windows,
            self.budget.as_secs()
        )
    }
}

/// What one seed's campaign came to.
///
/// Three outcomes, never two. Folding "the harness never ran" into "nothing was found" is
/// what let a deleted oracle report agreement for an afternoon.
enum Outcome {
    Clean { compared: usize },
    Diverged { commands: usize, report: String },
}

#[test]
fn differential_fuzz_against_sway() {
    if std::env::var_os("RUN_PARITY_FUZZ").is_none() {
        eprintln!("set RUN_PARITY_FUZZ=1 to run this; it needs sway on the machine");
        return;
    }

    let first_seed = std::env::var("PARITY_FUZZ_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(0x5EED);
    let budget = std::env::var("PARITY_FUZZ_SECONDS")
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(120));
    // `PARITY_FUZZ_TILING_ONLY` keeps its old name and its old meaning — the tree commands
    // alone — and `PARITY_FUZZ_NO_FLOATING` is the campaign it used to be mistaken for: the
    // whole tiled domain, fullscreen included.
    let domain = if std::env::var_os("PARITY_FUZZ_TILING_ONLY").is_some() {
        Domain::TREE_ONLY
    } else if std::env::var_os("PARITY_FUZZ_NO_FLOATING").is_some() {
        Domain::NO_FLOATING
    } else {
        Domain::EVERYTHING
    };
    let reference_name = std::env::var("PARITY_FUZZ_REFERENCE").unwrap_or_else(|_| "sway".into());
    if reference_name == "i3" && domain != Domain::TREE_ONLY {
        panic!("the i3 reference currently supports tiling-only campaigns");
    }

    let seeds: u64 = std::env::var("PARITY_FUZZ_SEEDS")
        .ok()
        .and_then(|count| count.parse().ok())
        .unwrap_or(1);

    let space = Space::current(domain, budget);
    eprintln!("\ncampaign against {reference_name}: {}\n", space.render());

    let mut failures = Vec::new();
    for seed in first_seed..first_seed + seeds {
        match run_seed(&reference_name, domain, budget, seed) {
            Outcome::Clean { compared } => {
                eprintln!("  seed {seed:#x}  clean, {compared} scripts compared");
            }
            Outcome::Diverged { commands, report } => {
                eprintln!("  seed {seed:#x}  DIVERGED, shrunk to {commands} commands");
                failures.push(report);
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "\n{} of {seeds} seed(s) diverged in this space — {}\n\n{}",
            failures.len(),
            space.render(),
            failures.join("\n\n")
        );
    }
}

/// One seed's campaign. Panics only when the harness itself is broken; a divergence is a
/// value, so the orchestrator can run the rest of the seeds and report them together.
fn run_seed(reference_name: &str, domain: Domain, budget: Duration, seed: u64) -> Outcome {
    // Derived from the recordings, so the ledger stays the only place a divergence is
    // described. Without this the search stops at the first thing it re-finds, and since a
    // known one is usually reachable in a handful of commands, that is all it ever does.
    let known = known::signatures();

    let mut rng = Rng(seed);
    let mut reference = Reference::start(reference_name)
        .unwrap_or_else(|err| panic!("cannot start {reference_name}: {err}"));
    let started = Instant::now();
    let mut scripts = 0usize;
    let mut compared = 0usize;
    let mut skipped = 0usize;
    let mut reference_failures = 0usize;
    let mut found = None;

    while started.elapsed() < budget && found.is_none() {
        let script = generate(&mut rng, domain);
        scripts += 1;
        match compare(&mut reference, &script) {
            Ok(None) => compared += 1,
            Ok(Some(divergence)) if known.contains(&divergence.signature) => {
                compared += 1;
                skipped += 1;
            }
            Ok(Some(divergence)) => {
                compared += 1;
                found = Some((script, divergence.signature));
            }
            Err(err) => {
                let script = shrink_reference_failure(reference_name, script, &err);
                if is_known_reference_failure(reference_name, &script, &err) {
                    reference_failures += 1;
                    reference = Reference::start(reference_name).unwrap_or_else(|restart| {
                        panic!("cannot restart {reference_name} after its known crash: {restart}")
                    });
                    continue;
                }
                let listing = script.iter().fold(String::new(), |mut out, command| {
                    let _ = writeln!(out, "{command}");
                    out
                });
                panic!(
                    "the reference or harness broke after {scripts} scripts; reduced to:\n\n\
                     {listing}\n{err}"
                );
            }
        }
    }

    let Some((script, signature)) = found else {
        // A campaign that never compared anything is not evidence that anything agrees, and
        // it looks exactly like one that compared thousands: same silence, same green test.
        // That is how a stale oracle path turned into a reported "no divergence" — the whole
        // budget spent failing to start sway, and nobody the wiser. So the only quiet exit is
        // the one that did the work.
        assert!(
            compared > 0,
            "the campaign finished without comparing a single script against {reference_name} \
             ({scripts} generated, {reference_failures} reference failures). That is a broken \
             harness reporting agreement, not agreement."
        );
        if skipped > 0 || reference_failures > 0 {
            eprintln!(
                "            ({skipped} already in the ledger, {reference_failures} known \
                 reference failures)"
            );
        }
        return Outcome::Clean { compared };
    };

    let script = shrink(&mut reference, script, &signature).expect("shrinking failed");
    let report = compare(&mut reference, &script).expect("the shrunk script stopped running");

    let Some(divergence) = report else {
        panic!("the divergence vanished while shrinking; that is a bug in the harness");
    };

    let listing: String = script.iter().fold(String::new(), |mut out, command| {
        let _ = writeln!(out, "{command}");
        out
    });

    let promoted = promote_to_fixture(seed, &listing).map_or_else(
        || {
            "Save that script as tiri-parity/fixtures/<name>.parity and record it, or rerun \
             with PARITY_FUZZ_PROMOTE=1 to have it written for you."
                .to_owned()
        },
        |path| format!("Written to {path}. Record it with:\n  cargo run -p tiri-parity --bin record -- {path}"),
    );

    Outcome::Diverged {
        commands: script.len(),
        report: format!(
            "seed {seed:#x}: divergence after {scripts} scripts, {skipped} already known, \
             shrunk to {} commands:\n\n\
             {listing}\n\
             step {} — after `{}`:\n--- {reference_name} ---\n{}--- tiri ---\n{}\n\
             {promoted}",
            script.len(),
            divergence.step + 1,
            divergence.command,
            divergence.expected.render(),
            divergence.actual.render(),
        ),
    }
}

/// Write a found script where the recorder can pick it up.
///
/// The step between finding a divergence and having a fixture used to be hand work: copy the
/// script out of a panic message, write the header the reader wants, get the name right. It
/// is the step that repeats on every single find, so it is the one worth not doing by hand —
/// and the one where a mistyped header cost a recording session already.
fn promote_to_fixture(seed: u64, listing: &str) -> Option<String> {
    std::env::var_os("PARITY_FUZZ_PROMOTE")?;
    let path = format!("tiri-parity/fixtures/found-seed-{seed:x}.parity");
    // `$ ` per command: what the fuzz prints is a listing for a human, what the reader wants
    // is a script, and the difference is one prefix. Writing the listing straight out produced
    // a file the corpus refused to load.
    let script: String = listing.lines().fold(String::new(), |mut out, command| {
        let _ = writeln!(out, "$ {command}");
        out
    });
    let body = format!(
        "# Found by the differential fuzz, seed {seed:#x}, and shrunk. Replace this note with \
         what the divergence turned out to be — the fixture is the recording, the note is why \
         anyone should care.\n{script}"
    );
    std::fs::write(&path, body).ok()?;
    Some(path)
}
