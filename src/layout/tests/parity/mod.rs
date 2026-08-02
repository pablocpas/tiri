//! Differential parity testing against i3/sway.
//!
//! See `docs/design/parity.md`. Scripts are written in i3's command grammar, replayed
//! against tiri, and observed through the same IPC projection real clients see. What is
//! compared is the normalized observable model, never tree shape.

mod replay;
mod script;

pub(crate) use replay::replay;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod fuzz;
#[cfg(test)]
mod tests;
