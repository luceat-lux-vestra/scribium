//! `scribium-test-support` — Test utilities for Scribium.
//!
//! Provides:
//! - Fixture loading from `fixtures/` directories
//! - Golden test assertion helpers
//! - Temporary project builder for integration tests
//! - Normalized path and output comparison
//! - Quarkdown conformance corpus harness

use scribium_core as core;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points to the crate directory (crates/scribium-test-support)
    // Go up two levels to reach the workspace root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .expect("could not determine workspace root")
}

fn fixtures_dir() -> PathBuf {
    workspace_root().join("fixtures")
}

/// Load a fixture file by name from `fixtures/{category}/`.
pub fn load_fixture(category: &str, name: &str) -> String {
    let path = fixtures_dir().join(category).join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot load fixture {:?}: {}", path, e))
}

/// Assertion helper for golden tests.
/// Compares actual output against an expected file.
pub fn assert_golden(actual: &str, expected_path: &str) {
    let expected = std::fs::read_to_string(expected_path)
        .unwrap_or_else(|e| panic!("cannot read golden {:?}: {}", expected_path, e));
    assert_eq!(actual, expected, "golden mismatch for {}", expected_path);
}

/// Quarkdown conformance case metadata.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ConformanceCaseMeta {
    pub id: String,
    pub feature: String,
    pub compatibility_level: String,
    pub specification_source: String,
    pub description: String,
    #[serde(default)]
    pub known_divergence: Option<String>,
}

/// A conformance test case from the corpus.
#[derive(Debug, Clone)]
pub struct ConformanceCase {
    pub meta: ConformanceCaseMeta,
    pub input: String,
    pub case_dir: PathBuf,
}

impl ConformanceCase {
    /// Load a conformance case by ID from the corpus.
    pub fn load(case_id: &str) -> Self {
        let cases_dir = fixtures_dir().join("quarkdown-conformance/cases");
        let case_dir = cases_dir.join(case_id);
        let meta_path = case_dir.join("case.toml");
        let input_path = case_dir.join("input.qd");

        let meta_content = std::fs::read_to_string(&meta_path)
            .unwrap_or_else(|e| panic!("cannot read case metadata {:?}: {}", meta_path, e));
        let meta: ConformanceCaseMeta = toml::from_str(&meta_content)
            .unwrap_or_else(|e| panic!("cannot parse case metadata {:?}: {}", meta_path, e));

        let input = std::fs::read_to_string(&input_path)
            .unwrap_or_else(|e| panic!("cannot read case input {:?}: {}", input_path, e));

        Self {
            meta,
            input,
            case_dir,
        }
    }

    /// Get all case IDs in the corpus.
    pub fn list_all() -> Vec<String> {
        let cases_dir = fixtures_dir().join("quarkdown-conformance/cases");
        let mut ids = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&cases_dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                    && entry.path().join("case.toml").exists()
                {
                    ids.push(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
        ids.sort();
        ids
    }

    /// Compile the case input using Scribium core and return the result.
    pub fn compile(&self) -> core::CompileResult {
        let project = core::VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", &self.input)
            .expect("valid path")
            .build()
            .unwrap();
        core::compile(&project, &core::CompileOptions::default())
    }

    /// Verify the case compiles without unexpected diagnostics.
    /// Returns the compile result for further assertions.
    pub fn verify_parses(&self) -> core::CompileResult {
        let result = self.compile();
        // For "Parsed" level and above, we expect no parser errors (E2xxx)
        // but may have evaluation diagnostics (E3xxx) for unresolved variables
        let parser_errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.code.starts_with("E2"))
            .collect();
        assert!(
            parser_errors.is_empty(),
            "Case '{}' produced parser errors: {:?}",
            self.meta.id,
            parser_errors
        );
        result
    }
}

/// Run all conformance cases in the corpus.
pub fn run_all_conformance_cases() {
    for case_id in ConformanceCase::list_all() {
        let case = ConformanceCase::load(&case_id);
        let _result = case.verify_parses();
        println!("✓ {} ({})", case.meta.id, case.meta.feature);
        // TODO: Add more detailed verification based on compatibility_level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_conformance_cases() {
        let cases = ConformanceCase::list_all();
        assert!(
            !cases.is_empty(),
            "should have at least one conformance case"
        );
        println!("Found cases: {:?}", cases);
    }

    #[test]
    fn test_load_call_dot_prefixed_basic() {
        let case = ConformanceCase::load("call-dot-prefixed-basic");
        assert_eq!(case.meta.id, "call-dot-prefixed-basic");
        assert_eq!(case.meta.feature, "dot-prefixed-call");
        assert_eq!(case.meta.compatibility_level, "Parsed");
        assert!(!case.input.is_empty());
    }

    #[test]
    fn test_load_call_positional_basic() {
        let case = ConformanceCase::load("call-positional-basic");
        assert_eq!(case.meta.id, "call-positional-basic");
        assert_eq!(case.meta.feature, "positional-arguments");
        assert_eq!(case.meta.compatibility_level, "Parsed");
        assert!(!case.input.is_empty());
    }

    #[test]
    fn test_load_call_indented_body_basic() {
        let case = ConformanceCase::load("call-indented-body-basic");
        assert_eq!(case.meta.id, "call-indented-body-basic");
        assert_eq!(case.meta.feature, "indented-body");
        assert_eq!(case.meta.compatibility_level, "Parsed");
        assert!(!case.input.is_empty());
    }

    #[test]
    fn test_verify_dot_prefixed_parses() {
        let case = ConformanceCase::load("call-dot-prefixed-basic");
        let result = case.verify_parses();
        assert!(!result.ir.nodes.is_empty());
    }

    #[test]
    fn test_verify_positional_parses() {
        let case = ConformanceCase::load("call-positional-basic");
        let result = case.verify_parses();
        assert!(!result.ir.nodes.is_empty());
    }

    #[test]
    fn test_verify_indented_body_parses() {
        let case = ConformanceCase::load("call-indented-body-basic");
        let result = case.verify_parses();
        assert!(!result.ir.nodes.is_empty());
    }

    #[test]
    fn test_baseline_consistency() {
        // Verify that the supported baseline in upstream.toml matches
        // the explicitly declared reference baseline in documentation files.
        // This test extracts the declared baseline from specific patterns
        // to avoid false positives from historical version references.
        let root = workspace_root();

        // Read upstream.toml (authoritative source)
        let upstream_toml = root.join("docs/compatibility/quarkdown/upstream.toml");
        let upstream_content = std::fs::read_to_string(&upstream_toml)
            .unwrap_or_else(|e| panic!("cannot read upstream.toml: {}", e));
        let upstream: toml::Value = upstream_content
            .parse()
            .unwrap_or_else(|e| panic!("cannot parse upstream.toml: {}", e));
        let baseline_from_toml = upstream["upstream"]["supported_baseline"]
            .as_str()
            .expect("upstream.supported_baseline not found in upstream.toml");

        // Helper to extract declared baseline from a document
        let version_re = regex::Regex::new(r"v\d+\.\d+\.\d+").unwrap();
        fn extract_declared_baseline(
            content: &str,
            patterns: &[&str],
            version_re: &regex::Regex,
        ) -> Option<String> {
            for pattern in patterns {
                if let Some(idx) = content.find(pattern) {
                    let after = &content[idx + pattern.len()..];
                    if let Some(mat) = version_re.find(after) {
                        return Some(mat.as_str().to_string());
                    }
                }
            }
            None
        }

        // SPEC_SOURCES.md: "Reference version: Quarkdown **vX.Y.Z**"
        let spec_sources = root.join("docs/compatibility/quarkdown/SPEC_SOURCES.md");
        let spec_content = std::fs::read_to_string(&spec_sources)
            .unwrap_or_else(|e| panic!("cannot read SPEC_SOURCES.md: {}", e));
        let spec_patterns = ["Reference version:", "Reference baseline:"];
        let spec_baseline = extract_declared_baseline(&spec_content, &spec_patterns, &version_re)
            .expect("SPEC_SOURCES.md should declare a reference baseline");
        assert_eq!(
            spec_baseline, baseline_from_toml,
            "SPEC_SOURCES.md declared baseline ({}) should match upstream.toml ({})",
            spec_baseline, baseline_from_toml
        );

        // README.md: "reference baseline vX.Y.Z" or "Reference upstream: Quarkdown vX.Y.Z"
        let readme = root.join("docs/compatibility/quarkdown/README.md");
        let readme_content = std::fs::read_to_string(&readme)
            .unwrap_or_else(|e| panic!("cannot read compatibility README.md: {}", e));
        let readme_patterns = ["reference baseline", "Reference upstream:"];
        let readme_baseline =
            extract_declared_baseline(&readme_content, &readme_patterns, &version_re)
                .expect("compatibility README.md should declare a reference baseline");
        assert_eq!(
            readme_baseline, baseline_from_toml,
            "compatibility README.md declared baseline ({}) should match upstream.toml ({})",
            readme_baseline, baseline_from_toml
        );

        // Root README.md
        let root_readme = root.join("README.md");
        let root_readme_content = std::fs::read_to_string(&root_readme)
            .unwrap_or_else(|e| panic!("cannot read root README.md: {}", e));
        // Use only the Quarkdown-specific pattern to avoid matching Scribium milestone versions
        // Pattern excludes the "v" prefix since the version regex expects it
        let root_patterns = ["referenced against Quarkdown "];
        let root_baseline =
            extract_declared_baseline(&root_readme_content, &root_patterns, &version_re)
                .expect("root README.md should declare a reference baseline");
        assert_eq!(
            root_baseline, baseline_from_toml,
            "root README.md declared baseline ({}) should match upstream.toml ({})",
            root_baseline, baseline_from_toml
        );

        println!("✓ Baseline consistency verified: {}", baseline_from_toml);
    }
}
