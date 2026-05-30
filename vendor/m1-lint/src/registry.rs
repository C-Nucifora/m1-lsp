//! Rule registry — collects all active rules.

use crate::config::Config;
use crate::diagnostic::LintCode;
use crate::rules::Rule;

/// Holds all registered lint rules.
pub struct Registry {
    pub(crate) rules: Vec<Box<dyn Rule>>,
}

impl Registry {
    /// Create an empty registry.
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    /// Register a rule.
    pub fn register(&mut self, rule: Box<dyn Rule>) {
        self.rules.push(rule);
    }

    /// Returns the default registry for m1-lint v1.
    pub fn default_v1() -> Self {
        let mut r = Self::empty();
        r.register(Box::new(
            crate::rules::l001_line_too_long::LineTooLong::default(),
        ));
        r.register(Box::new(
            crate::rules::l002_trailing_whitespace::TrailingWhitespace,
        ));
        r.register(Box::new(
            crate::rules::l003_missing_final_newline::MissingFinalNewline,
        ));
        r.register(Box::new(
            crate::rules::l004_eq_operator_preferred::EqOperatorPreferred,
        ));
        r.register(Box::new(
            crate::rules::l005_logical_operator_preferred::LogicalOperatorPreferred,
        ));
        r.register(Box::new(
            crate::rules::l006_float_eq_comparison::FloatEqComparison,
        ));
        r.register(Box::new(
            crate::rules::l007_operator_spacing::OperatorSpacing,
        ));
        r.register(Box::new(
            crate::rules::l008_nesting_too_deep::NestingTooDeep::default(),
        ));
        r.register(Box::new(
            crate::rules::l009_cyclomatic_complexity::CyclomaticComplexity::default(),
        ));
        r
    }

    /// All registered rules.
    pub fn rules(&self) -> &[Box<dyn Rule>] {
        &self.rules
    }

    /// The full v2 rule set with default thresholds.
    pub fn default_v2() -> Self {
        Self::from_config(&Config::default())
    }

    /// Build a registry containing exactly the rules enabled by `cfg`, with
    /// the configured thresholds.
    pub fn from_config(cfg: &Config) -> Self {
        use crate::rules::*;
        let mut r = Self::empty();
        for code in &cfg.enabled {
            match code {
                LintCode::L001 => r.register(Box::new(l001_line_too_long::LineTooLong {
                    max_len: cfg.max_line_length,
                })),
                LintCode::L002 => {
                    r.register(Box::new(l002_trailing_whitespace::TrailingWhitespace))
                }
                LintCode::L003 => {
                    r.register(Box::new(l003_missing_final_newline::MissingFinalNewline))
                }
                LintCode::L004 => {
                    r.register(Box::new(l004_eq_operator_preferred::EqOperatorPreferred))
                }
                LintCode::L005 => r.register(Box::new(
                    l005_logical_operator_preferred::LogicalOperatorPreferred,
                )),
                LintCode::L006 => r.register(Box::new(l006_float_eq_comparison::FloatEqComparison)),
                LintCode::L007 => r.register(Box::new(l007_operator_spacing::OperatorSpacing)),
                LintCode::L008 => r.register(Box::new(l008_nesting_too_deep::NestingTooDeep {
                    max_depth: cfg.max_nesting_depth,
                })),
                LintCode::L009 => {
                    r.register(Box::new(l009_cyclomatic_complexity::CyclomaticComplexity {
                        max_complexity: cfg.max_complexity,
                    }))
                }
                LintCode::L010 => r.register(Box::new(l010_tab_indentation::TabIndentation)),
                LintCode::L011 => r.register(Box::new(l011_comment_style::CommentStyle)),
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_has_no_rules() {
        let r = Registry::empty();
        assert_eq!(r.rules().len(), 0);
    }

    #[test]
    fn from_config_respects_select() {
        let mut cfg = crate::config::Config::default();
        cfg.apply_filters(Some(vec!["L004".into()]), None).unwrap();
        let r = Registry::from_config(&cfg);
        assert_eq!(r.rules().len(), 1);
        assert_eq!(r.rules()[0].code(), LintCode::L004);
    }
}
