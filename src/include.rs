use std::fs;
use std::path::Path;

use anyhow::Result;
use globset::{GlobSet, GlobSetBuilder};

/// The include file name looked for in the project directory.
pub const INCLUDE_FILE: &str = ".mpyinclude";

/// A filter built from a `.mpyinclude` file that determines which files
/// are considered for syncing / uploading / diffing.
pub struct IncludeFilter {
    matcher: GlobSet,
}

impl IncludeFilter {
    /// Look for `.mpyinclude` in the given directory and build a filter.
    /// Returns `Ok(None)` if the file doesn't exist.
    pub fn load(dir: &Path) -> Result<Option<IncludeFilter>> {
        let include_path = dir.join(INCLUDE_FILE);
        if !include_path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&include_path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", include_path.display(), e))?;

        let filter = Self::parse(&content)?;
        Ok(Some(filter))
    }

    /// Parse `.mpyinclude` content into an `IncludeFilter`.
    ///
    /// Format:
    /// - One glob pattern per line
    /// - `#` starts a comment (inline comments supported)
    /// - Blank lines are ignored
    /// - Patterns without `/` match against filename at any depth (e.g. `*.py`)
    /// - Patterns with `/` match against the relative path (e.g. `src/*.py`)
    pub fn parse(content: &str) -> Result<IncludeFilter> {
        let mut builder = GlobSetBuilder::new();
        let mut has_pattern = false;

        for line in content.lines() {
            // Strip comments
            let line = match line.find('#') {
                Some(i) => &line[..i],
                None => line,
            };
            let line = line.trim();

            if line.is_empty() {
                continue;
            }

            // If the pattern contains no '/', it's a filename-only pattern.
            // Prepend `**/` so it matches at any depth (like .gitignore).
            let pattern = if !line.contains('/') {
                format!("**/{}", line)
            } else {
                line.to_string()
            };

            let glob = globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| anyhow::anyhow!("invalid glob pattern '{}': {}", line, e))?;
            builder.add(glob);
            has_pattern = true;
        }

        if !has_pattern {
            anyhow::bail!(".mpyinclude contains no valid patterns");
        }

        let matcher = builder
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build glob matcher: {}", e))?;

        Ok(IncludeFilter { matcher })
    }

    /// Check if a relative path (e.g., `src/main.py`, `config.json`)
    /// matches any include pattern.
    pub fn is_match(&self, rel_path: &str) -> bool {
        self.matcher.is_match(rel_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_extension() {
        let filter = IncludeFilter::parse("*.py\n").unwrap();
        assert!(filter.is_match("main.py"));
        assert!(filter.is_match("src/main.py"));
        assert!(filter.is_match("src/sub/main.py"));
        assert!(!filter.is_match("main.js"));
    }

    #[test]
    fn test_specific_path() {
        let filter = IncludeFilter::parse("src/*.py\n").unwrap();
        assert!(filter.is_match("src/main.py"));
        assert!(!filter.is_match("main.py"));
        assert!(!filter.is_match("src/sub/main.py"));
    }

    #[test]
    fn test_recursive_glob() {
        let filter = IncludeFilter::parse("src/**/*.py\n").unwrap();
        assert!(filter.is_match("src/main.py"));
        assert!(filter.is_match("src/sub/main.py"));
        assert!(!filter.is_match("main.py"));
    }

    #[test]
    fn test_comments_and_blanks() {
        let filter = IncludeFilter::parse("# comment\n\n*.py\n  # another\n").unwrap();
        assert!(filter.is_match("main.py"));
    }

    #[test]
    fn test_multiple_patterns() {
        let filter = IncludeFilter::parse("*.py\n*.json\nconfig/*\n").unwrap();
        assert!(filter.is_match("main.py"));
        assert!(filter.is_match("data.json"));
        assert!(filter.is_match("config/settings.ini"));
        assert!(!filter.is_match("README.md"));
    }

    #[test]
    fn test_no_patterns_errors() {
        assert!(IncludeFilter::parse("# only comments\n\n").is_err());
    }
}
