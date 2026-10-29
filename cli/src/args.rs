//! A small `--name value` argument parser, enough for these tools without a new dependency.

use std::collections::HashMap;
use std::str::FromStr;

use crate::CliError;

/// Parsed arguments: positional words, then `--name value` options and `--flag`s.
#[derive(Debug, Default)]
pub struct Args {
    positional: Vec<String>,
    options: HashMap<String, String>,
    flags: Vec<String>,
}

impl Args {
    /// Parses `args` (without the program name). `flags` lists the options that take no value.
    ///
    /// # Errors
    ///
    /// `Args` for an option without a value.
    pub fn parse(args: impl IntoIterator<Item = String>, flags: &[&str]) -> Result<Self, CliError> {
        let mut out = Self::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            match a.strip_prefix("--") {
                Some(name) if flags.contains(&name) => out.flags.push(name.to_owned()),
                Some(name) => {
                    let value = it
                        .next()
                        .ok_or_else(|| CliError::Args(format!("--{name} needs a value")))?;
                    out.options.insert(name.to_owned(), value);
                }
                None => out.positional.push(a),
            }
        }
        Ok(out)
    }

    /// The `i`-th positional word.
    #[must_use]
    pub fn positional(&self, i: usize) -> Option<&str> {
        self.positional.get(i).map(String::as_str)
    }

    /// Whether `--name` was given.
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    /// The raw value of `--name`, if given.
    #[must_use]
    pub fn opt(&self, name: &str) -> Option<&str> {
        self.options.get(name).map(String::as_str)
    }

    /// The value of a required `--name`.
    ///
    /// # Errors
    ///
    /// `Args` if it is missing.
    pub fn req(&self, name: &str) -> Result<&str, CliError> {
        self.opt(name)
            .ok_or_else(|| CliError::Args(format!("missing --{name}")))
    }

    /// `--name` parsed as `T`, or `default` if absent.
    ///
    /// # Errors
    ///
    /// `Args` if present and unparsable.
    pub fn parse_or<T: FromStr>(&self, name: &str, default: T) -> Result<T, CliError> {
        match self.opt(name) {
            Some(v) => v
                .parse()
                .map_err(|_| CliError::Args(format!("--{name}: cannot parse {v:?}"))),
            None => Ok(default),
        }
    }

    /// A required `--name` parsed as `T`.
    ///
    /// # Errors
    ///
    /// `Args` if missing or unparsable.
    pub fn parse_req<T: FromStr>(&self, name: &str) -> Result<T, CliError> {
        let v = self.req(name)?;
        v.parse()
            .map_err(|_| CliError::Args(format!("--{name}: cannot parse {v:?}")))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_macros)]
mod tests {
    use super::Args;

    #[test]
    fn test_args_parse_positional_options_and_flags() {
        let a = Args::parse(
            ["settle", "--size", "5", "--dry-run", "--side", "buy"].map(String::from),
            &["dry-run"],
        )
        .unwrap();
        assert_eq!(a.positional(0), Some("settle"));
        assert_eq!(a.parse_req::<u64>("size").unwrap(), 5);
        assert_eq!(a.req("side").unwrap(), "buy");
        assert!(a.flag("dry-run"));
        assert!(a.req("missing").is_err());
        assert_eq!(a.parse_or("ttl", 30u64).unwrap(), 30);
        assert!(Args::parse(["--size".to_string()], &[]).is_err());
    }
}
