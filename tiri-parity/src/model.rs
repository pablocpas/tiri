//! The observable layout model.
//!
//! This is what a user can perceive of a workspace: which window has focus, where each
//! window is, whether it is visible or hidden behind a tab, and in what order. Anything a
//! compositor is free to represent differently without the user noticing is deliberately
//! absent, so that two implementations agreeing on behaviour produce equal models even when
//! their internal trees differ.

use std::fmt::Write as _;

/// How a container arranges its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    SplitH,
    SplitV,
    Tabbed,
    Stacked,
}

impl Layout {
    /// Parse sway's `layout` field. `output` and `none` are not workspace layouts.
    pub fn from_sway(name: &str) -> Option<Self> {
        match name {
            "splith" => Some(Layout::SplitH),
            "splitv" => Some(Layout::SplitV),
            "tabbed" => Some(Layout::Tabbed),
            "stacked" => Some(Layout::Stacked),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Layout::SplitH => "splith",
            Layout::SplitV => "splitv",
            Layout::Tabbed => "tabbed",
            Layout::Stacked => "stacked",
        }
    }
}

/// Windows are identified by the order the harness opened them, starting at 1.
///
/// Never by pid, Wayland id or title: those differ between runs and between compositors,
/// and comparing them would make every fixture single-use.
pub type WindowId = u32;

/// A rectangle as fractions of the working area.
///
/// Pixels are not comparable across compositors: gaps, borders and title bars are
/// configuration, not behaviour. Fractions are.
#[derive(Debug, Clone, Copy)]
pub struct FracRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// How close two fractions must be to count as equal. Generous enough to absorb integer
/// pixel rounding at 1080p, tight enough that a real layout difference cannot hide.
const EPS: f64 = 2e-3;

impl FracRect {
    pub fn approx_eq(self, other: Self) -> bool {
        (self.x - other.x).abs() < EPS
            && (self.y - other.y).abs() < EPS
            && (self.w - other.w).abs() < EPS
            && (self.h - other.h).abs() < EPS
    }
}

#[derive(Debug, Clone)]
pub enum Node {
    Window(Window),
    Container(Container),
}

#[derive(Debug, Clone)]
pub struct Window {
    pub id: WindowId,
    pub rect: FracRect,
    /// False when the window is hidden behind a tab or a stack.
    pub visible: bool,
    pub floating: bool,
    pub marks: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Container {
    pub layout: Layout,
    pub rect: FracRect,
    pub marks: Vec<String>,
    pub nodes: Vec<Node>,
}

/// What holds focus.
///
/// A container can, which is what `focus parent` leaves behind, and it matters: two states
/// that differ only in *which* container is selected send the next command somewhere else.
/// Recording only the focused window would make those states indistinguishable, and a
/// difference nothing can express is a difference nothing can find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    /// An empty workspace, with nothing to focus.
    Nothing,
    Window(WindowId),
    /// A container, addressed by position. The workspace is itself a container, and it is
    /// the empty path.
    Container(Vec<usize>),
}

impl Focus {
    fn render(&self) -> String {
        match self {
            Focus::Nothing => "none".into(),
            Focus::Window(id) => id.to_string(),
            Focus::Container(path) => {
                let mut out = String::from("@");
                for (idx, step) in path.iter().enumerate() {
                    if idx > 0 {
                        out.push('/');
                    }
                    let _ = write!(out, "{step}");
                }
                out
            }
        }
    }

    fn parse(field: &str) -> Option<Self> {
        if field == "none" {
            return Some(Focus::Nothing);
        }
        let Some(path) = field.strip_prefix('@') else {
            return field.parse().ok().map(Focus::Window);
        };
        if path.is_empty() {
            return Some(Focus::Container(Vec::new()));
        }
        path.split('/')
            .map(|step| step.parse().ok())
            .collect::<Option<Vec<usize>>>()
            .map(Focus::Container)
    }
}

/// A workspace: a container with an orientation, plus what is focused.
///
/// Both compositors normalize into this. In sway the workspace node already is such a
/// container; in tiri the tree root plus the workspace layout describe the same thing.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub layout: Layout,
    pub focused: Focus,
    pub nodes: Vec<Node>,
}

/// Where two models differ, in terms a person can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// Position in the model, e.g. `workspace/1/0`.
    pub at: String,
    pub expected: String,
    pub actual: String,
}

impl Workspace {
    /// Compare two models, returning every difference rather than the first.
    ///
    /// Seeing all of them at once is what makes a divergence diagnosable; stopping at the
    /// first turns one command's worth of difference into several rounds of guessing.
    pub fn diff(&self, other: &Workspace) -> Vec<Difference> {
        let mut out = Vec::new();
        if self.layout != other.layout {
            out.push(Difference {
                at: "workspace".into(),
                expected: self.layout.as_str().into(),
                actual: other.layout.as_str().into(),
            });
        }
        if self.focused != other.focused {
            out.push(Difference {
                at: "workspace/focus".into(),
                expected: self.focused.render(),
                actual: other.focused.render(),
            });
        }
        diff_nodes("workspace", &self.nodes, &other.nodes, &mut out);
        out
    }

    /// A stable one-line-per-node rendering, for snapshots and failure messages.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "workspace {} focus={}",
            self.layout.as_str(),
            self.focused.render()
        );
        render_nodes(&self.nodes, 1, &mut out);
        out
    }
}

fn render_nodes(nodes: &[Node], depth: usize, out: &mut String) {
    for node in nodes {
        let pad = "  ".repeat(depth);
        match node {
            Node::Window(w) => {
                let mut flags = String::new();
                if !w.visible {
                    flags.push_str(" hidden");
                }
                if w.floating {
                    flags.push_str(" floating");
                }
                for mark in &w.marks {
                    let _ = write!(flags, " mark:{mark}");
                }
                let _ = writeln!(out, "{pad}window {} {}{}", w.id, render_rect(w.rect), flags);
            }
            Node::Container(c) => {
                let mut flags = String::new();
                for mark in &c.marks {
                    let _ = write!(flags, " mark:{mark}");
                }
                let _ = writeln!(
                    out,
                    "{pad}{} {}{}",
                    c.layout.as_str(),
                    render_rect(c.rect),
                    flags
                );
                render_nodes(&c.nodes, depth + 1, out);
            }
        }
    }
}

fn render_rect(r: FracRect) -> String {
    format!("{:.3},{:.3} {:.3}x{:.3}", r.x, r.y, r.w, r.h)
}

fn diff_nodes(at: &str, expected: &[Node], actual: &[Node], out: &mut Vec<Difference>) {
    if expected.len() != actual.len() {
        out.push(Difference {
            at: at.into(),
            expected: format!("{} children", expected.len()),
            actual: format!("{} children", actual.len()),
        });
    }

    for (idx, (e, a)) in expected.iter().zip(actual).enumerate() {
        let at = format!("{at}/{idx}");
        match (e, a) {
            (Node::Window(e), Node::Window(a)) => {
                if e.id != a.id {
                    push(
                        out,
                        &at,
                        format!("window {}", e.id),
                        format!("window {}", a.id),
                    );
                }
                if !e.rect.approx_eq(a.rect) {
                    push(out, &at, render_rect(e.rect), render_rect(a.rect));
                }
                if e.visible != a.visible {
                    push(out, &at, vis(e.visible), vis(a.visible));
                }
                if e.floating != a.floating {
                    push(out, &at, float(e.floating), float(a.floating));
                }
                if e.marks != a.marks {
                    push(out, &at, format!("{:?}", e.marks), format!("{:?}", a.marks));
                }
            }
            (Node::Container(e), Node::Container(a)) => {
                if e.layout != a.layout {
                    push(out, &at, e.layout.as_str().into(), a.layout.as_str().into());
                }
                if !e.rect.approx_eq(a.rect) {
                    push(out, &at, render_rect(e.rect), render_rect(a.rect));
                }
                if e.marks != a.marks {
                    push(out, &at, format!("{:?}", e.marks), format!("{:?}", a.marks));
                }
                diff_nodes(&at, &e.nodes, &a.nodes, out);
            }
            (Node::Window(e), Node::Container(a)) => {
                push(
                    out,
                    &at,
                    format!("window {}", e.id),
                    a.layout.as_str().into(),
                );
            }
            (Node::Container(e), Node::Window(a)) => {
                push(
                    out,
                    &at,
                    e.layout.as_str().into(),
                    format!("window {}", a.id),
                );
            }
        }
    }
}

fn push(out: &mut Vec<Difference>, at: &str, expected: String, actual: String) {
    out.push(Difference {
        at: at.into(),
        expected,
        actual,
    });
}

fn vis(v: bool) -> String {
    if v { "visible" } else { "hidden" }.into()
}

fn float(v: bool) -> String {
    if v { "floating" } else { "tiled" }.into()
}

/// Read back what [`Workspace::render`] wrote.
///
/// Fixtures are stored as rendered text so a recording is reviewable in a diff, but they
/// have to come back as a model to be compared with the geometry tolerance rather than
/// character by character. Round-tripping is what keeps the two directions honest.
pub fn parse(text: &str) -> Result<Workspace, ParseError> {
    let mut lines = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty());

    let (header_no, header) = lines.next().ok_or(ParseError {
        line: 0,
        reason: "empty",
    })?;
    let mut header = header.split_whitespace();
    if header.next() != Some("workspace") {
        return Err(ParseError {
            line: header_no + 1,
            reason: "expected a workspace line",
        });
    }
    let layout = header
        .next()
        .and_then(Layout::from_sway)
        .ok_or(ParseError {
            line: header_no + 1,
            reason: "unknown workspace layout",
        })?;
    let focused = header
        .next()
        .and_then(|field| field.strip_prefix("focus="))
        .and_then(Focus::parse)
        .ok_or(ParseError {
            line: header_no + 1,
            reason: "unreadable focus",
        })?;

    // Depth is carried by indentation, so a stack of the containers still open is enough.
    let mut root = Vec::new();
    let mut open: Vec<(usize, Container)> = Vec::new();
    for (idx, line) in lines {
        let no = idx + 1;
        let depth = (line.len() - line.trim_start().len()) / 2;
        if depth == 0 {
            return Err(ParseError {
                line: no,
                reason: "node outside the workspace",
            });
        }
        while open.len() >= depth {
            close_one(&mut open, &mut root);
        }
        if open.len() + 1 != depth {
            return Err(ParseError {
                line: no,
                reason: "indentation skips a level",
            });
        }
        match parse_node(line.trim_start(), no)? {
            Node::Container(container) => open.push((depth, container)),
            node => attach(&mut open, &mut root, node),
        }
    }
    while !open.is_empty() {
        close_one(&mut open, &mut root);
    }

    Ok(Workspace {
        layout,
        focused,
        nodes: root,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub reason: &'static str,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.reason)
    }
}

fn close_one(open: &mut Vec<(usize, Container)>, root: &mut Vec<Node>) {
    let Some((_, container)) = open.pop() else {
        return;
    };
    attach(open, root, Node::Container(container));
}

fn attach(open: &mut [(usize, Container)], root: &mut Vec<Node>, node: Node) {
    match open.last_mut() {
        Some((_, container)) => container.nodes.push(node),
        None => root.push(node),
    }
}

fn parse_node(line: &str, no: usize) -> Result<Node, ParseError> {
    let mut fields = line.split_whitespace();
    let kind = fields.next().ok_or(ParseError {
        line: no,
        reason: "empty node",
    })?;
    let err = |reason| ParseError { line: no, reason };

    if kind == "window" {
        let id = fields
            .next()
            .and_then(|id| id.parse().ok())
            .ok_or(err("unreadable window id"))?;
        let rect = parse_rect(&mut fields, no)?;
        let mut window = Window {
            id,
            rect,
            visible: true,
            floating: false,
            marks: Vec::new(),
        };
        for flag in fields {
            match flag {
                "hidden" => window.visible = false,
                "floating" => window.floating = true,
                _ => match flag.strip_prefix("mark:") {
                    Some(mark) => window.marks.push(mark.to_owned()),
                    None => return Err(err("unknown window flag")),
                },
            }
        }
        return Ok(Node::Window(window));
    }

    let layout = Layout::from_sway(kind).ok_or(err("unknown node kind"))?;
    let rect = parse_rect(&mut fields, no)?;
    let mut marks = Vec::new();
    for flag in fields {
        match flag.strip_prefix("mark:") {
            Some(mark) => marks.push(mark.to_owned()),
            None => return Err(err("unknown container flag")),
        }
    }
    Ok(Node::Container(Container {
        layout,
        rect,
        marks,
        nodes: Vec::new(),
    }))
}

fn parse_rect<'a>(
    fields: &mut impl Iterator<Item = &'a str>,
    no: usize,
) -> Result<FracRect, ParseError> {
    let err = ParseError {
        line: no,
        reason: "unreadable rectangle",
    };
    let loc = fields.next().ok_or(err.clone())?;
    let size = fields.next().ok_or(err.clone())?;
    let (x, y) = loc.split_once(',').ok_or(err.clone())?;
    let (w, h) = size.split_once('x').ok_or(err.clone())?;
    Ok(FracRect {
        x: x.parse().map_err(|_| err.clone())?,
        y: y.parse().map_err(|_| err.clone())?,
        w: w.parse().map_err(|_| err.clone())?,
        h: h.parse().map_err(|_| err)?,
    })
}
