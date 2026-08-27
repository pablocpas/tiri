//! Options this build parses but does not act on.
//!
//! A fork inherits its config language along with its code, and drops parts of the code
//! faster than parts of the language. What is left accepts a value and reserves nothing,
//! which is worse than either keeping the option or refusing it: the user writes it, the
//! parser agrees, and nothing happens.
//!
//! Removing them would refuse configs that load today, so they keep parsing and say so
//! instead. An option is only named when it was actually set to something other than its
//! default — a warning about a line nobody wrote is noise, and noise is how a warning stops
//! being read.

use crate::Config;

/// One option that is accepted and ignored.
pub struct IgnoredOption {
    /// Where it lives, spelled the way the user wrote it.
    pub path: &'static str,
    /// Why it does nothing, in terms of what tiri is rather than what it is not.
    pub reason: &'static str,
}

impl Config {
    /// Every option this config sets that tiri parses and then ignores.
    ///
    /// Empty for a config that sets none of them, which is the common case.
    pub fn ignored_options(&self) -> Vec<IgnoredOption> {
        let mut out = Vec::new();

        if self.layout.tab_indicator != Default::default() {
            out.push(IgnoredOption {
                path: "layout { tab-indicator }",
                reason: "it configures niri's indicator strip beside a scrolling column. \
                         Tiri draws i3-style tab bars instead, configured by `layout { tab-bar }`",
            });
        }

        if self
            .window_rules
            .iter()
            .any(|rule| rule.tab_indicator != Default::default())
        {
            out.push(IgnoredOption {
                path: "window-rule { tab-indicator }",
                reason: "same indicator, per window. `layout { tab-bar }` is not overridable \
                         per window",
            });
        }

        if self
            .input
            .focus_follows_mouse
            .is_some_and(|ffm| ffm.max_scroll_amount.is_some())
        {
            out.push(IgnoredOption {
                path: "input { focus-follows-mouse max-scroll-amount }",
                reason: "it bounds how far a scrolling viewport would travel to reach a \
                         window, and a tiling tree never scrolls to one",
            });
        }

        if self.debug.disable_transactions {
            out.push(IgnoredOption {
                path: "debug { disable-transactions }",
                reason: "nothing reads it; transactions cannot be turned off",
            });
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_that_sets_none_of_them_is_warned_about_nothing() {
        let config = Config::parse_mem("").unwrap();
        assert!(config.ignored_options().is_empty());
    }

    #[test]
    fn the_default_config_is_warned_about_nothing() {
        // The example config ships as the thing to copy. If it triggers a warning, the
        // warning is wrong or the example is.
        let config = Config::parse_mem(include_str!("../../resources/default-config.kdl"))
            .expect("the default config parses");
        let ignored: Vec<_> = config
            .ignored_options()
            .iter()
            .map(|i| i.path)
            .collect();
        assert!(ignored.is_empty(), "{ignored:?}");
    }

    #[test]
    fn setting_an_ignored_option_names_it() {
        let config = Config::parse_mem(
            r#"
            layout {
                tab-indicator {
                    width 8
                }
            }
            debug {
                disable-transactions
            }
            "#,
        )
        .unwrap();

        let paths: Vec<_> = config
            .ignored_options()
            .iter()
            .map(|i| i.path)
            .collect();
        assert_eq!(
            paths,
            ["layout { tab-indicator }", "debug { disable-transactions }"]
        );
    }

    #[test]
    fn an_ignored_option_left_at_its_default_is_not_named() {
        // Writing the block out without changing anything asks for what tiri already does.
        let config = Config::parse_mem(
            r#"
            layout {
                tab-indicator {
                }
            }
            "#,
        )
        .unwrap();
        assert!(config.ignored_options().is_empty());
    }
}
