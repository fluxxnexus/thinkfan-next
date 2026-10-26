//! YAML loading and parsing, followed by validation.
use super::Config;
use anyhow::{Context, Result};
use std::{fs, path::Path};
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let input =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&input).with_context(|| format!("YAML configuration {}", path.display()))
    }
    pub fn parse(input: &str) -> Result<Self> {
        let config: Self = noyalib_serde_yaml::from_str(input)
            .context("expected a YAML sensors/fans/levels mapping")?;
        config.validate()?;
        Ok(config)
    }
}
