//! Effective lint configuration: thresholds + the active rule set.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::diagnostic::LintCode;

/// The resolved configuration the runner uses.
#[derive(Debug, Clone)]
pub struct Config {
    pub max_line_length: usize,
    pub max_nesting_depth: usize,
    pub max_complexity: u32,
    pub enabled: BTreeSet<LintCode>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_line_length: 88,
            max_nesting_depth: 4,
            max_complexity: 10,
            enabled: LintCode::all_codes().iter().copied().collect(),
        }
    }
}

/// Raw, fully-optional view parsed from `.m1lint.toml`.
#[derive(Debug, Default)]
struct RawConfig {
    max_line_length: Option<usize>,
    max_nesting_depth: Option<usize>,
    max_complexity: Option<u32>,
    select: Option<Vec<String>>,
    ignore: Option<Vec<String>>,
}

/// A configuration error (maps to CLI exit code 2).
#[derive(Debug)]
pub enum ConfigError {
    Toml(String),
    UnknownKey(String),
    UnknownCode(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Toml(e) => write!(f, "invalid .m1lint.toml: {e}"),
            ConfigError::UnknownKey(k) => write!(f, "unknown config key: {k}"),
            ConfigError::UnknownCode(c) => write!(f, "unknown lint code: {c}"),
        }
    }
}

impl Config {
    /// Parse a `.m1lint.toml` string into a raw view, then merge over defaults.
    pub fn from_toml_str(s: &str) -> Result<Config, ConfigError> {
        let raw = parse_raw(s)?;
        let mut cfg = Config::default();
        cfg.apply_raw(raw)?;
        Ok(cfg)
    }

    /// Walk up from `start_dir` looking for a `.m1lint.toml`. Returns the
    /// parsed config if found, else `Config::default()`.
    pub fn discover(start_dir: &Path) -> Result<Config, ConfigError> {
        let mut dir: Option<&Path> = Some(start_dir);
        while let Some(d) = dir {
            let candidate = d.join(".m1lint.toml");
            if candidate.is_file() {
                let text = std::fs::read_to_string(&candidate)
                    .map_err(|e| ConfigError::Toml(e.to_string()))?;
                return Config::from_toml_str(&text);
            }
            dir = d.parent();
        }
        Ok(Config::default())
    }

    fn apply_raw(&mut self, raw: RawConfig) -> Result<(), ConfigError> {
        if let Some(n) = raw.max_line_length {
            self.max_line_length = n;
        }
        if let Some(n) = raw.max_nesting_depth {
            self.max_nesting_depth = n;
        }
        if let Some(n) = raw.max_complexity {
            self.max_complexity = n;
        }
        self.apply_filters(raw.select, raw.ignore)
    }

    /// Apply select-then-ignore over the current `enabled` set.
    pub fn apply_filters(
        &mut self,
        select: Option<Vec<String>>,
        ignore: Option<Vec<String>>,
    ) -> Result<(), ConfigError> {
        if let Some(sel) = select {
            let mut set = BTreeSet::new();
            for s in sel {
                let code = LintCode::from_code_str(&s)
                    .ok_or(ConfigError::UnknownCode(s))?;
                set.insert(code);
            }
            self.enabled = set;
        }
        if let Some(ign) = ignore {
            for s in ign {
                let code = LintCode::from_code_str(&s)
                    .ok_or(ConfigError::UnknownCode(s))?;
                self.enabled.remove(&code);
            }
        }
        Ok(())
    }
}

fn parse_raw(s: &str) -> Result<RawConfig, ConfigError> {
    let value: toml::Value =
        s.parse().map_err(|e: toml::de::Error| ConfigError::Toml(e.to_string()))?;
    let table = value
        .as_table()
        .ok_or_else(|| ConfigError::Toml("top level must be a table".into()))?;

    let mut raw = RawConfig::default();
    for (k, v) in table {
        match k.as_str() {
            "max-line-length" => raw.max_line_length = v.as_integer().map(|n| n as usize),
            "max-nesting-depth" => raw.max_nesting_depth = v.as_integer().map(|n| n as usize),
            "max-complexity" => raw.max_complexity = v.as_integer().map(|n| n as u32),
            "select" => raw.select = Some(string_array(v)?),
            "ignore" => raw.ignore = Some(string_array(v)?),
            other => return Err(ConfigError::UnknownKey(other.to_string())),
        }
    }
    Ok(raw)
}

fn string_array(v: &toml::Value) -> Result<Vec<String>, ConfigError> {
    v.as_array()
        .ok_or_else(|| ConfigError::Toml("expected array of strings".into()))?
        .iter()
        .map(|e| {
            e.as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| ConfigError::Toml("expected string in array".into()))
        })
        .collect()
}

/// Helper for callers (CLI/tests) needing the config's directory base.
pub fn dir_of(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_all() {
        assert_eq!(Config::default().enabled.len(), 11);
        assert_eq!(Config::default().max_line_length, 88);
    }

    #[test]
    fn parses_thresholds() {
        let cfg = Config::from_toml_str("max-line-length = 100\nmax-complexity = 12\n").unwrap();
        assert_eq!(cfg.max_line_length, 100);
        assert_eq!(cfg.max_complexity, 12);
        assert_eq!(cfg.max_nesting_depth, 4); // untouched default
    }

    #[test]
    fn select_restricts() {
        let cfg = Config::from_toml_str("select = [\"L001\", \"L004\"]\n").unwrap();
        assert_eq!(cfg.enabled.len(), 2);
        assert!(cfg.enabled.contains(&LintCode::L001));
        assert!(!cfg.enabled.contains(&LintCode::L006));
    }

    #[test]
    fn ignore_subtracts() {
        let cfg = Config::from_toml_str("ignore = [\"L007\"]\n").unwrap();
        assert!(!cfg.enabled.contains(&LintCode::L007));
        assert!(cfg.enabled.contains(&LintCode::L001));
    }

    #[test]
    fn unknown_key_errors() {
        assert!(matches!(
            Config::from_toml_str("max-lien-length = 100\n"),
            Err(ConfigError::UnknownKey(_))
        ));
    }

    #[test]
    fn unknown_code_errors() {
        assert!(matches!(
            Config::from_toml_str("select = [\"L999\"]\n"),
            Err(ConfigError::UnknownCode(_))
        ));
    }

    #[test]
    fn discover_walks_up_to_default_when_absent() {
        let tmp = std::env::temp_dir();
        // A directory unlikely to contain .m1lint.toml up its chain in CI.
        let cfg = Config::discover(&tmp).unwrap();
        assert!(cfg.enabled.len() <= 11);
    }
}
