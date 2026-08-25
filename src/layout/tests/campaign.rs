//! A campaign over the random layout operations.
//!
//! `random_operations_dont_panic` is a gate: it stops at the first counterexample, shrinks it
//! and aborts, so one run answers "is there a bug" and never "how many". Worse for a search,
//! proptest persists the seed it failed on and replays it before generating anything new, so
//! running the gate in a loop finds the same bug until someone fixes it.
//!
//! This runs the same generator against the same invariants and does not stop. Every panic is
//! caught, grouped by the assertion that fired, and the shortest sequence reaching each one is
//! shrunk and reported. The output is a list of distinct findings, not a verdict — which is
//! what you want while the layout is still being built, and what the sway-parity fuzzer next
//! door already does for divergences.
//!
//! ```sh
//! RUN_LAYOUT_CAMPAIGN=1 cargo test --release --lib -- campaign --nocapture
//! ```
//!
//! `LAYOUT_CAMPAIGN_CASES` sets how many cases to generate (default 20000) and
//! `LAYOUT_CAMPAIGN_SEED` makes a run reproducible.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Mutex;

use proptest::strategy::{Strategy as _, ValueTree as _};
use proptest::test_runner::{Config as ProptestConfig, RngAlgorithm, TestRng, TestRunner};

use super::*;

/// Where a panic came from and what it said.
///
/// The site is the grouping key: two counterexamples that fire the same assertion are one
/// finding to fix, however differently they got there. The message travels with it because
/// several invariants share a line — `verify_invariants` asserts more than one thing — and
/// because the message is what a reader recognises.
///
/// But the message names the windows it caught, and those ids are whatever the generator drew.
/// Left alone that splits one bug into an entry per draw: the first campaign reported five
/// findings that were two. So the key holds the message with its numbers blanked, and the
/// message as written is kept beside it for the report.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Site {
    location: String,
    shape: String,
}

impl Site {
    fn new(location: String, message: &str) -> Self {
        let mut shape = String::with_capacity(message.len());
        let mut in_number = false;
        for ch in message.chars() {
            if ch.is_ascii_digit() {
                if !in_number {
                    shape.push('N');
                    in_number = true;
                }
            } else {
                shape.push(ch);
                in_number = false;
            }
        }
        Self { location, shape }
    }
}

/// One case that reached a site.
struct Finding {
    site: Site,
    message: String,
    hits: usize,
    ops: Vec<Op>,
    layout_config: tiri_config::LayoutPart,
}

/// The panic hook's drop box.
///
/// A hook cannot return a value, so it leaves the site here and `catch_unwind` picks it up.
/// The hook also prints nothing: a campaign that reports a thousand hits would otherwise
/// print a thousand backtraces before the summary anyone wants to read.
static LAST_PANIC: Mutex<Option<(Site, String)>> = Mutex::new(None);

fn run_case(ops: &[Op], layout_config: &tiri_config::LayoutPart) -> Option<(Site, String)> {
    let options = Options {
        layout: tiri_config::Layout::from_part(layout_config),
        ..Default::default()
    };

    LAST_PANIC.lock().unwrap().take();
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        check_ops_with_options(options, ops.to_vec());
    }));

    if outcome.is_ok() {
        return None;
    }

    // A panic with no hook record still counts — better an unnamed finding than a silent one.
    Some(LAST_PANIC.lock().unwrap().take().unwrap_or_else(|| {
        let message = "<panic without a recorded location>".to_string();
        (Site::new("<unknown>".into(), &message), message)
    }))
}

/// The reductions to try on a single op, in the order worth trying them.
///
/// Dropping ops alone leaves a sequence nobody can paste into a test: the generator draws
/// bboxes like `-26044 x 52011`, min/max sizes, and a kilobyte of resolved window rules, and
/// none of that is usually what the bug is about. Each of these replaces one op with a plainer
/// one; the caller keeps it only if the same assertion still fires, so whatever survives is
/// load-bearing and whatever goes was noise.
fn simpler_forms(op: &Op) -> Vec<Op> {
    fn plain(params: &TestWindowParams) -> Vec<TestWindowParams> {
        let mut forms = Vec::new();
        // The whole thing at its default, keeping only what a reader would have written.
        let mut bare = TestWindowParams::new(params.id);
        bare.is_floating = params.is_floating;
        // Spelled out rather than `!=` because a candidate equal to what we already have would
        // "succeed" every pass and never let the loop settle.
        let differs = params.rules.is_some()
            || params.bbox != bare.bbox
            || params.min_max_size != bare.min_max_size
            || params.is_urgent
            || params.parent_id.is_some();
        if differs {
            forms.push(bare.clone());
        }
        // Then one field at a time, for the cases where the default went too far.
        if params.rules.is_some() {
            forms.push(TestWindowParams {
                rules: None,
                ..params.clone()
            });
        }
        if params.bbox != bare.bbox {
            forms.push(TestWindowParams {
                bbox: bare.bbox,
                ..params.clone()
            });
        }
        if params.min_max_size != bare.min_max_size {
            forms.push(TestWindowParams {
                min_max_size: bare.min_max_size,
                ..params.clone()
            });
        }
        if params.is_urgent {
            forms.push(TestWindowParams {
                is_urgent: false,
                ..params.clone()
            });
        }
        if params.parent_id.is_some() {
            forms.push(TestWindowParams {
                parent_id: None,
                ..params.clone()
            });
        }
        forms
    }

    match op {
        Op::AddWindow { params } => plain(params)
            .into_iter()
            .map(|params| Op::AddWindow { params })
            .collect(),
        Op::AddWindowNextTo { params, next_to_id } => plain(params)
            .into_iter()
            .map(|params| Op::AddWindowNextTo {
                params,
                next_to_id: *next_to_id,
            })
            .collect(),
        Op::AddWindowToNamedWorkspace { params, ws_name } => plain(params)
            .into_iter()
            .map(|params| Op::AddWindowToNamedWorkspace {
                params,
                ws_name: *ws_name,
            })
            .collect(),
        Op::AddScaledOutput {
            id,
            scale,
            layout_config,
        } => {
            let mut forms = Vec::new();
            if layout_config.is_some() {
                forms.push(Op::AddScaledOutput {
                    id: *id,
                    scale: *scale,
                    layout_config: None,
                });
            }
            forms
        }
        Op::UpdateOutputLayoutConfig { id, layout_config } => layout_config
            .is_some()
            .then_some(Op::UpdateOutputLayoutConfig {
                id: *id,
                layout_config: None,
            })
            .into_iter()
            .collect(),
        Op::AddNamedWorkspace {
            ws_name,
            output_name,
            layout_config,
        } => layout_config
            .is_some()
            .then_some(Op::AddNamedWorkspace {
                ws_name: *ws_name,
                output_name: *output_name,
                layout_config: None,
            })
            .into_iter()
            .collect(),
        Op::UpdateWorkspaceLayoutConfig {
            ws_name,
            layout_config,
        } => layout_config
            .is_some()
            .then_some(Op::UpdateWorkspaceLayoutConfig {
                ws_name: *ws_name,
                layout_config: None,
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// Cut the case down while it still reaches the same site.
///
/// Deliberately not proptest's shrinker: that one shrinks towards *any* failure, and here a
/// case that starts failing somewhere else has stopped being an example of this finding. So
/// every move is checked against the same assertion firing again.
///
/// Two kinds of move, alternating to a fixed point: drop an op, and replace an op with a
/// plainer one. Dropping alone gives a short sequence full of generated noise; simplifying
/// alone leaves ops that do nothing. Together they give something you can paste into a test.
fn shrink(
    site: &Site,
    ops: &[Op],
    layout_config: &tiri_config::LayoutPart,
) -> (Vec<Op>, tiri_config::LayoutPart) {
    let mut ops = ops.to_vec();
    let mut layout_config = layout_config.clone();

    let reaches = |ops: &[Op], layout_config: &tiri_config::LayoutPart| {
        run_case(ops, layout_config)
            .map(|(site, _)| site)
            .as_ref()
            == Some(site)
    };

    // The case's own layout config first: it applies to every op, so losing it is the single
    // biggest cut available.
    if layout_config != tiri_config::LayoutPart::default()
        && reaches(&ops, &tiri_config::LayoutPart::default())
    {
        layout_config = tiri_config::LayoutPart::default();
    }

    let mut progress = true;
    while progress {
        progress = false;

        let mut idx = 0;
        while idx < ops.len() {
            let mut candidate = ops.clone();
            candidate.remove(idx);
            if reaches(&candidate, &layout_config) {
                ops = candidate;
                progress = true;
            } else {
                idx += 1;
            }
        }

        for idx in 0..ops.len() {
            for form in simpler_forms(&ops[idx]) {
                let mut candidate = ops.clone();
                candidate[idx] = form;
                if reaches(&candidate, &layout_config) {
                    ops = candidate;
                    progress = true;
                    break;
                }
            }
        }
    }

    (ops, layout_config)
}

#[test]
fn collect_every_panic_the_random_ops_reach() {
    if std::env::var_os("RUN_LAYOUT_CAMPAIGN").is_none() {
        eprintln!("set RUN_LAYOUT_CAMPAIGN=1 to run the layout campaign");
        return;
    }

    let cases: u32 = std::env::var("LAYOUT_CAMPAIGN_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(20000);
    let seed: u64 = std::env::var("LAYOUT_CAMPAIGN_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(0x5EED);

    // The generator the gate uses, verbatim. A campaign that searched a different space would
    // not be answering for the test whose failures it is standing in for.
    let strategy = (any::<Vec<Op>>(), arbitrary_layout_part());

    // ChaCha takes a 32-byte seed; the knob is one number so a run can be named in a sentence.
    let mut seed_bytes = [0u8; 32];
    seed_bytes[..8].copy_from_slice(&seed.to_le_bytes());

    let mut runner = TestRunner::new_with_rng(
        ProptestConfig {
            cases,
            ..ProptestConfig::default()
        },
        TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes),
    );

    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map_or_else(|| "<unknown>".to_string(), |loc| format!("{loc}"));
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic>".into());
        *LAST_PANIC.lock().unwrap() = Some((Site::new(location, &message), message));
    }));

    let mut findings: HashMap<Site, Finding> = HashMap::new();
    let mut red = 0usize;
    let mut generated = 0usize;

    for _ in 0..cases {
        let Ok(tree) = strategy.new_tree(&mut runner) else {
            continue;
        };
        let (ops, layout_config) = tree.current();
        generated += 1;

        let Some((site, message)) = run_case(&ops, &layout_config) else {
            continue;
        };
        red += 1;

        findings
            .entry(site.clone())
            .and_modify(|finding| {
                finding.hits += 1;
                // Keep the shortest example: it is the one worth spending the shrink on.
                if ops.len() < finding.ops.len() {
                    finding.ops = ops.clone();
                    finding.layout_config = layout_config.clone();
                }
            })
            .or_insert_with(|| Finding {
                site,
                message,
                hits: 1,
                ops: ops.clone(),
                layout_config: layout_config.clone(),
            });
    }

    // Shrink once per finding rather than once per hit, which is the whole point of grouping.
    let mut findings: Vec<_> = findings.into_values().collect();
    for finding in &mut findings {
        let (ops, layout_config) = shrink(&finding.site, &finding.ops, &finding.layout_config);
        finding.ops = ops;
        finding.layout_config = layout_config;
    }
    findings.sort_by(|a, b| {
        b.hits
            .cmp(&a.hits)
            .then_with(|| a.site.location.cmp(&b.site.location))
    });

    panic::set_hook(previous_hook);

    eprintln!();
    eprintln!("campaign: {generated} cases generated, seed {seed:#x}");
    eprintln!(
        "{} distinct site(s), {red} red case(s)",
        findings.len()
    );
    eprintln!();

    for (idx, finding) in findings.iter().enumerate() {
        eprintln!(
            "[{}] {}  x{}  minimal {} op(s)",
            idx + 1,
            finding.site.location,
            finding.hits,
            finding.ops.len(),
        );
        eprintln!("    {}", finding.message);
        eprintln!("    ops = {:?}", finding.ops);
        eprintln!("    layout_config = {:?}", finding.layout_config);
        eprintln!();
    }

    if findings.is_empty() {
        eprintln!("clean.");
    }
}
