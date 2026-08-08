use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Parser, Debug)]
#[command(
    name = "scribium-upstream-watch",
    version,
    about = "Upstream release drift detector for Scribium"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Check if an observed release differs from the supported baseline
    CheckRelease {
        /// Path to the upstream manifest TOML file
        #[arg(long, value_name = "PATH")]
        manifest: PathBuf,
        /// Observed release tag (e.g., v2.6.0)
        #[arg(long, value_name = "TAG")]
        observed_tag: String,
        /// Observed release URL
        #[arg(long, value_name = "URL")]
        observed_url: String,
        /// Output format (json or text)
        #[arg(long, value_name = "FORMAT", default_value = "json")]
        output: OutputFormat,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    Json,
    Text,
}

#[derive(Debug, Deserialize)]
struct UpstreamManifest {
    schema_version: u32,
    upstream: UpstreamInfo,
}

#[derive(Debug, Deserialize)]
struct UpstreamInfo {
    id: String,
    #[allow(dead_code)]
    repository: String,
    #[allow(dead_code)]
    release_channel: String,
    supported_baseline: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Current,
    Drift,
}

#[derive(Debug, Serialize)]
struct CheckResult {
    upstream: String,
    supported_baseline: String,
    observed_tag: String,
    status: Status,
    issue_key: Option<String>,
}

#[derive(Error, Debug)]
enum WatchError {
    #[error("Failed to read manifest file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to parse manifest TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("Invalid manifest: schema_version must be 1, got {0}")]
    InvalidSchemaVersion(u32),
    #[error("Manifest missing required field: {0}")]
    #[allow(dead_code)]
    MissingField(String),
    #[error("Observed tag cannot be empty")]
    EmptyObservedTag,
    #[error("Observed URL cannot be empty")]
    EmptyObservedUrl,
}

fn main() -> Result<(), WatchError> {
    let cli = Cli::parse();

    match cli.command {
        Commands::CheckRelease {
            manifest,
            observed_tag,
            observed_url,
            output,
        } => {
            if observed_tag.trim().is_empty() {
                return Err(WatchError::EmptyObservedTag);
            }
            if observed_url.trim().is_empty() {
                return Err(WatchError::EmptyObservedUrl);
            }

            let content = std::fs::read_to_string(&manifest)?;
            let manifest_data: UpstreamManifest = toml::from_str(&content)?;

            if manifest_data.schema_version != 1 {
                return Err(WatchError::InvalidSchemaVersion(
                    manifest_data.schema_version,
                ));
            }

            let upstream = &manifest_data.upstream;
            let supported_baseline = &upstream.supported_baseline;

            let status = if observed_tag == *supported_baseline {
                Status::Current
            } else {
                Status::Drift
            };

            let issue_key = if status == Status::Drift {
                Some(format!("{}:{}", upstream.id, observed_tag))
            } else {
                None
            };

            let result = CheckResult {
                upstream: upstream.id.clone(),
                supported_baseline: supported_baseline.clone(),
                observed_tag,
                status,
                issue_key,
            };

            match output {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&result).unwrap());
                }
                OutputFormat::Text => {
                    println!("Upstream: {}", result.upstream);
                    println!("Supported baseline: {}", result.supported_baseline);
                    println!("Observed tag: {}", result.observed_tag);
                    println!("Status: {:?}", result.status);
                    if let Some(key) = result.issue_key {
                        println!("Issue key: {}", key);
                    }
                }
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn create_test_manifest(dir: &std::path::Path, baseline: &str) -> PathBuf {
        let manifest_content = format!(
            r#"
schema_version = 1

[upstream]
id = "quarkdown"
repository = "iamgio/quarkdown"
release_channel = "stable"
supported_baseline = "{}"
"#,
            baseline
        );
        let manifest_path = dir.join("upstream.toml");
        fs::write(&manifest_path, manifest_content).unwrap();
        manifest_path
    }

    #[test]
    fn test_current_status() {
        let dir = tempdir().unwrap();
        let manifest = create_test_manifest(dir.path(), "v2.5.0");

        let result = run_check_release(
            &manifest,
            "v2.5.0",
            "https://github.com/iamgio/quarkdown/releases/tag/v2.5.0",
            OutputFormat::Json,
        );
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("\"status\": \"current\""));
        assert!(output.contains("\"issue_key\": null"));
    }

    #[test]
    fn test_drift_status() {
        let dir = tempdir().unwrap();
        let manifest = create_test_manifest(dir.path(), "v2.5.0");

        let result = run_check_release(
            &manifest,
            "v2.6.0",
            "https://github.com/iamgio/quarkdown/releases/tag/v2.6.0",
            OutputFormat::Json,
        );
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("\"status\": \"drift\""));
        assert!(output.contains("\"issue_key\": \"quarkdown:v2.6.0\""));
    }

    #[test]
    fn test_invalid_schema_version() {
        let dir = tempdir().unwrap();
        let manifest_path = dir.path().join("upstream.toml");
        let manifest_content = r#"
schema_version = 2

[upstream]
id = "quarkdown"
repository = "iamgio/quarkdown"
release_channel = "stable"
supported_baseline = "v2.5.0"
"#;
        fs::write(&manifest_path, manifest_content).unwrap();

        let result = run_check_release(
            &manifest_path,
            "v2.5.0",
            "https://example.com",
            OutputFormat::Json,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("schema_version must be 1"));
    }

    #[test]
    fn test_missing_baseline() {
        let dir = tempdir().unwrap();
        let manifest_path = dir.path().join("upstream.toml");
        let manifest_content = r#"
schema_version = 1

[upstream]
id = "quarkdown"
repository = "iamgio/quarkdown"
release_channel = "stable"
"#;
        fs::write(&manifest_path, manifest_content).unwrap();

        let result = run_check_release(
            &manifest_path,
            "v2.5.0",
            "https://example.com",
            OutputFormat::Json,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_observed_tag() {
        let dir = tempdir().unwrap();
        let manifest = create_test_manifest(dir.path(), "v2.5.0");

        let result = run_check_release(&manifest, "", "https://example.com", OutputFormat::Json);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Observed tag cannot be empty"));
    }

    #[test]
    fn test_empty_observed_url() {
        let dir = tempdir().unwrap();
        let manifest = create_test_manifest(dir.path(), "v2.5.0");

        let result = run_check_release(&manifest, "v2.6.0", "", OutputFormat::Json);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Observed URL cannot be empty"));
    }

    #[test]
    fn test_issue_key_deterministic() {
        let dir = tempdir().unwrap();
        let manifest = create_test_manifest(dir.path(), "v2.5.0");

        let result1 = run_check_release(
            &manifest,
            "v2.6.0",
            "https://github.com/iamgio/quarkdown/releases/tag/v2.6.0",
            OutputFormat::Json,
        );
        let result2 = run_check_release(
            &manifest,
            "v2.6.0",
            "https://github.com/iamgio/quarkdown/releases/tag/v2.6.0",
            OutputFormat::Json,
        );

        assert!(result1.is_ok());
        assert!(result2.is_ok());
        let output1 = result1.unwrap();
        let output2 = result2.unwrap();
        assert_eq!(output1, output2);
        assert!(output1.contains("\"issue_key\": \"quarkdown:v2.6.0\""));
    }

    fn run_check_release(
        manifest: &PathBuf,
        observed_tag: &str,
        observed_url: &str,
        format: OutputFormat,
    ) -> Result<String, WatchError> {
        let content = std::fs::read_to_string(manifest)?;
        let manifest_data: UpstreamManifest = toml::from_str(&content)?;

        if manifest_data.schema_version != 1 {
            return Err(WatchError::InvalidSchemaVersion(
                manifest_data.schema_version,
            ));
        }

        let upstream = &manifest_data.upstream;
        let supported_baseline = &upstream.supported_baseline;

        if observed_tag.trim().is_empty() {
            return Err(WatchError::EmptyObservedTag);
        }
        if observed_url.trim().is_empty() {
            return Err(WatchError::EmptyObservedUrl);
        }

        let status = if observed_tag == *supported_baseline {
            Status::Current
        } else {
            Status::Drift
        };

        let issue_key = if status == Status::Drift {
            Some(format!("{}:{}", upstream.id, observed_tag))
        } else {
            None
        };

        let result = CheckResult {
            upstream: upstream.id.clone(),
            supported_baseline: supported_baseline.clone(),
            observed_tag: observed_tag.to_string(),
            status,
            issue_key,
        };

        match format {
            OutputFormat::Json => Ok(serde_json::to_string_pretty(&result).unwrap()),
            OutputFormat::Text => {
                let mut output = String::new();
                output.push_str(&format!("Upstream: {}\n", result.upstream));
                output.push_str(&format!(
                    "Supported baseline: {}\n",
                    result.supported_baseline
                ));
                output.push_str(&format!("Observed tag: {}\n", result.observed_tag));
                output.push_str(&format!("Status: {:?}\n", result.status));
                if let Some(key) = result.issue_key {
                    output.push_str(&format!("Issue key: {}\n", key));
                }
                Ok(output)
            }
        }
    }
}
