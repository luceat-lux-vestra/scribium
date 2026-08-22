use anyhow::Context;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use scribium_project::{VirtualPathBuf, VirtualProject, VirtualProjectBuilder};
use scribium_typst::{TypstBackend, TypstInput};
use scribium_typst_subprocess::{SubprocessBackend, TypstSourceContext};

/// Represents a loaded project with both physical and virtual paths.
struct LoadedProject {
    project: VirtualProject,
    /// The path as requested by the user (logical path for output naming)
    requested_entry: PathBuf,
    /// Explicit physical source root passed to the native Typst backend.
    source_context: TypstSourceContext,
}
fn os_relative_path_to_virtual(path: &Path) -> anyhow::Result<VirtualPathBuf> {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| {
                    anyhow::anyhow!("path is not valid UTF-8: {}", path.display())
                })?;

                components.push(value);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("path is not project-relative: {}", path.display());
            }
        }
    }

    VirtualPathBuf::parse(components.join("/")).map_err(Into::into)
}

/// Returns the logical project root for an input path.
///
/// The project root is the parent directory of the requested entry. When the
/// request is a bare file name (e.g. `document.qd`) the parent is empty, and
/// the current directory `"."` is used instead. Returned for relative and
/// absolute paths alike; the caller decides how to resolve it.
fn logical_project_root(requested_entry: &Path) -> PathBuf {
    requested_entry
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// Supported input extensions. Typst passthrough (`.typ`) is not implemented
/// yet, so it is deliberately excluded and rejected at the CLI boundary.
/// Extension matching is ASCII case-insensitive (`.QD` is accepted).
const SUPPORTED_INPUT_EXTENSIONS: [&str; 3] = ["qd", "scrib", "md"];

/// Validates that `input` has a supported source extension.
///
/// Files without an extension are rejected, as are extensions outside
/// `.qd`/`.scrib`/`.md`. Matching is ASCII case-insensitive.
fn validate_input_extension(input: &Path) -> anyhow::Result<()> {
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if SUPPORTED_INPUT_EXTENSIONS.contains(&ext.as_str()) {
        Ok(())
    } else if ext.is_empty() {
        anyhow::bail!(
            "missing input extension '{}' (supported: qd, scrib, md)",
            input.display()
        );
    } else {
        anyhow::bail!(
            "unsupported input extension '.{}' (supported: qd, scrib, md)",
            ext
        );
    }
}

/// Loads the bounded project tree rooted at the entry's logical directory into
/// a VirtualProject. Filesystem access stays at this native host boundary;
/// compiler and evaluator code only see the resulting logical sources/assets.
fn load_single_file_project(input: &Path) -> anyhow::Result<LoadedProject> {
    validate_input_extension(input)?;
    // Store the user-requested path for output naming
    let requested_entry = input.to_path_buf();

    let physical_entry = input
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", input.display()))?;

    // Project root is based on requested path (logical root)
    let logical_project_root = logical_project_root(&requested_entry);

    // Canonicalized project root for symlink containment check
    let canonical_project_root = logical_project_root.canonicalize().with_context(|| {
        format!(
            "cannot resolve project root {}",
            logical_project_root.display()
        )
    })?;

    // Verify the physical entry is within the canonical project root (symlink escape check)
    if !physical_entry.starts_with(&canonical_project_root) {
        return Err(anyhow::anyhow!(
            "input file '{}' resolves to '{}' which is outside project root '{}' (symlink escape)",
            requested_entry.display(),
            physical_entry.display(),
            canonical_project_root.display()
        ));
    }

    // Compute logical virtual entry from requested path (not canonicalized)
    let requested_relative = if requested_entry
        .parent()
        .map(|parent| parent.as_os_str().is_empty())
        .unwrap_or(false)
    {
        // Bare file name: the logical root is "." which has no path components,
        // so strip_prefix would fail. Use the file name directly.
        PathBuf::from(requested_entry.file_name().ok_or_else(|| {
            anyhow::anyhow!("input has no file name: {}", requested_entry.display())
        })?)
    } else {
        requested_entry
            .strip_prefix(&logical_project_root)
            .map_err(|_| {
                anyhow::anyhow!(
                    "input is outside project root: {}",
                    requested_entry.display()
                )
            })?
            .to_path_buf()
    };
    let virtual_entry = os_relative_path_to_virtual(&requested_relative)?;

    let mut files = Vec::new();
    collect_project_files(&canonical_project_root, &canonical_project_root, &mut files)?;

    let mut builder = VirtualProjectBuilder::new().entry(virtual_entry.as_str())?;
    for (path, bytes) in files {
        let path = os_relative_path_to_virtual(&path)?;
        let source_extension = path
            .file_name()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, extension)| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "qd" | "scrib" | "md"
                )
            })
            .unwrap_or(false);
        if source_extension {
            let source = String::from_utf8(bytes.clone())
                .with_context(|| format!("source file is not valid UTF-8: {}", path.as_str()))?;
            builder = builder.add_source(path.as_str(), source)?;
        }
        builder = builder.add_asset(path.as_str(), bytes)?;
    }
    let project = builder.build()?;

    Ok(LoadedProject {
        project,
        requested_entry,
        source_context: TypstSourceContext::new(canonical_project_root),
    })
}

fn collect_project_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(current)
        .with_context(|| format!("cannot read project directory {}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("cannot enumerate project directory {}", current.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("cannot inspect project entry {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            let canonical = path
                .canonicalize()
                .with_context(|| format!("cannot resolve project symlink {}", path.display()))?;
            if !canonical.starts_with(root) {
                // An unrelated output/link entry must not make loading the
                // logical source project fail. If a document later refers to
                // this path, it is absent from the VirtualProject and fails
                // closed as a missing resource; Typst's own mirror boundary
                // separately rejects symlink escapes.
                continue;
            }
            if canonical.is_dir() {
                // Do not recurse through aliases: this avoids duplicate
                // logical trees and symlink cycles while keeping the loader
                // deterministic.
                continue;
            }
        }

        let canonical = path
            .canonicalize()
            .with_context(|| format!("cannot resolve project entry {}", path.display()))?;
        if !canonical.starts_with(root) {
            anyhow::bail!(
                "project entry '{}' resolves outside project root '{}'",
                path.display(),
                root.display()
            );
        }
        if metadata.is_dir() {
            collect_project_files(root, &path, files)?;
        } else if metadata.is_file() || metadata.file_type().is_symlink() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| anyhow::anyhow!("project entry is outside root: {}", path.display()))?
                .to_path_buf();
            let bytes = fs::read(&path)
                .with_context(|| format!("cannot read project resource {}", path.display()))?;
            files.push((relative, bytes));
        }
    }
    Ok(())
}

/// Compiles a pre-loaded VirtualProject.
fn compile_project(project: &VirtualProject) -> anyhow::Result<scribium_core::CompileResult> {
    let options = scribium_core::CompileOptions::default();
    Ok(scribium_core::compile(project, &options))
}
/// Returns an error if any diagnostic has Severity::Error.
fn ensure_no_errors(diagnostics: &[scribium_core::Diagnostic]) -> anyhow::Result<()> {
    let error_count = diagnostics
        .iter()
        .filter(|d| matches!(&d.severity, scribium_core::Severity::Error))
        .count();
    if error_count > 0 {
        anyhow::bail!("found {} error(s)", error_count);
    }
    Ok(())
}

/// Execute the `build` command: compile input to output format(s).
///
/// `typst_path` selects the Typst executable used for PDF output. It is only
/// consulted when a `pdf` format is requested; a `typst`-only build never
/// spawns a subprocess.
pub fn build(
    input: &str,
    formats: &[String],
    output: Option<&Path>,
    typst_path: &Path,
) -> anyhow::Result<()> {
    const SUPPORTED_FORMATS: &[&str] = &["typst", "pdf"];

    let unsupported: Vec<&String> = formats
        .iter()
        .filter(|f| !SUPPORTED_FORMATS.contains(&f.as_str()))
        .collect();
    if let Some(format) = unsupported.first() {
        anyhow::bail!(
            "output format '{}' is not yet implemented (supported: typst, pdf)",
            format
        );
    }
    if formats.is_empty() {
        anyhow::bail!("no output format requested");
    }

    let input_path = Path::new(input);
    let loaded = load_single_file_project(input_path)?;

    let result = compile_project(&loaded.project)?;

    for diag in &result.diagnostics {
        eprintln!("{:?}", diag);
    }

    // Fail on error diagnostics before writing output
    ensure_no_errors(&result.diagnostics)?;

    let typst_code = scribium_typst::lowering::lower_to_typst_code(&result.ir);

    // Determine output paths for each requested format
    let mut output_paths = Vec::new();
    for format in formats {
        let out_path = match (output, format.as_str()) {
            // If explicit output is given and only one format, use it
            (Some(path), _) if formats.len() == 1 => path.to_path_buf(),
            // If explicit output is given with multiple formats, that's an error
            (Some(_), _) => {
                anyhow::bail!(
                    "cannot use --output with multiple formats; specify one format or omit --output"
                );
            }
            // Default output path for each format
            (None, "typst") => default_typst_output_path(&loaded.requested_entry),
            (None, "pdf") => default_pdf_output_path(&loaded.requested_entry),
            (None, _) => unreachable!("validated above"),
        };
        output_paths.push((format.clone(), out_path));
    }

    // Check all output paths for collisions before any writes
    for (_, out_path) in &output_paths {
        ensure_distinct_output(&loaded.requested_entry, out_path)?;
        reject_lexically_colliding_output(&loaded.requested_entry, out_path)?;
    }

    // Resolve all output paths
    let mut resolved_paths = Vec::new();
    for (format, out_path) in output_paths {
        let resolved = resolve_output_path(&out_path)?;
        // Re-verify immediately before writing
        ensure_distinct_output(&loaded.requested_entry, &resolved)?;
        resolved_paths.push((format, resolved));
    }

    // Write outputs for each format
    for (format, resolved_out_path) in resolved_paths {
        match format.as_str() {
            "typst" => {
                write_output_atomically(&resolved_out_path, typst_code.as_bytes())?;
                eprintln!("Wrote generated Typst to {}", resolved_out_path.display());
            }
            "pdf" => {
                let typst_input = TypstInput {
                    source: typst_code.clone(),
                    entry_path: loaded.project.entry().as_str().to_string(),
                };
                let backend = SubprocessBackend::new(typst_path)
                    .with_source_context(loaded.source_context.clone());
                let typst_output = backend
                    .compile(&typst_input)
                    .map_err(|e| anyhow::anyhow!("PDF compilation failed: {}", e))?;
                if let Some(pdf_bytes) = typst_output.pdf {
                    write_output_atomically(&resolved_out_path, &pdf_bytes)?;
                    eprintln!("Wrote PDF to {}", resolved_out_path.display());
                } else {
                    anyhow::bail!("PDF backend did not produce output");
                }
            }
            _ => unreachable!("validated above"),
        }
    }

    Ok(())
}

/// Returns the default output path for Typst output.
/// Replaces the extension with `.typ`.
fn default_typst_output_path(requested_entry: &Path) -> PathBuf {
    requested_entry.with_extension("typ")
}

/// Returns the default output path for PDF output.
/// Replaces the extension with `.pdf`.
fn default_pdf_output_path(requested_entry: &Path) -> PathBuf {
    requested_entry.with_extension("pdf")
}

/// Creates the parent directories of `out_path` when they do not exist.
///
/// A bare file name has no parent and needs no creation. Any component that
/// exists as a regular file makes the operation fail with a clear error.
fn create_output_dirs(out_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create output directory {}", parent.display()))?;
        }
    }
    Ok(())
}

/// Resolves the effective output path for `out_path`.
///
/// Missing parent directories are created first, then the real parent
/// directory is canonicalized and recombined with the original file name.
/// `.`/`..` components and symlinks inside the output path are therefore
/// interpreted against the actual filesystem state after directory
/// creation; the result is the path the same-file check and the final
/// write both operate on. The caller is responsible for verifying that the
/// resolved path does not alias the input before writing.
fn resolve_output_path(out_path: &Path) -> anyhow::Result<PathBuf> {
    create_output_dirs(out_path)?;

    let parent = out_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("cannot resolve output directory '{}'", parent.display()))?;
    let name = out_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("output path has no file name: '{}'", out_path.display()))?;

    Ok(canonical_parent.join(name))
}

/// Why an output path could not be pre-resolved into an effective form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputPathResolutionError {
    /// Windows drive-relative path (`C:dir\file.typ`): its meaning depends
    /// on the per-drive current-directory state, which is not supported for
    /// CLI output paths.
    DriveRelativePath,
    /// The working directory could not be canonicalized, so relative output
    /// paths cannot be anchored.
    WorkingDirectoryUnavailable,
}

impl std::fmt::Display for OutputPathResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DriveRelativePath => write!(
                f,
                "drive-relative output paths are not supported; use 'C:\\...' or a path relative to the current directory"
            ),
            Self::WorkingDirectoryUnavailable => write!(
                f,
                "cannot resolve the output path: the current directory could not be determined"
            ),
        }
    }
}

impl std::error::Error for OutputPathResolutionError {}

/// Returns the drive prefix (`C:`) of an absolute Windows path, if any.
#[cfg(windows)]
fn path_drive_prefix(path: &Path) -> Option<std::ffi::OsString> {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => Some(prefix.as_os_str().to_os_string()),
        _ => None,
    }
}

/// Resolves the effective output path the filesystem would land on, without
/// creating anything.
///
/// Components are walked left to right: whenever the path-so-far exists it
/// is canonicalized, so symlinks resolve "as reached" and a `..` after a
/// symlink moves to the symlink target's parent. Non-existent suffixes are
/// kept on an in-memory stack — `..` canceling a non-existent component
/// never creates directories, and `..` above the filesystem root stays at
/// the root. Path kinds are classified explicitly:
///
/// * fully absolute (`C:\dir\file.typ`, UNC, verbatim) — anchored at their
///   own prefix and root;
/// * root-relative on Windows (`\dir\file.typ`) — anchored at the current
///   drive's root (the working directory's prefix);
/// * drive-relative (`C:dir\file.typ`) — unsupported, returned as an error;
/// * ordinary relative paths — anchored at the canonicalized working
///   directory.
///
/// Returns `Ok(Some(path))` when the effective path could be computed,
/// `Ok(None)` for a pathological path that cannot be compared (the caller
/// then relies on the canonicalized checks), and `Err` when a supported
/// path kind could not be resolved or the path kind is unsupported. The
/// result is not authoritative: the canonicalized same-file check
/// immediately before the write remains the final guard.
fn resolve_effective_output_without_creation(
    path: &Path,
    base: &Path,
) -> Result<Option<PathBuf>, OutputPathResolutionError> {
    let mut components = path.components().peekable();
    let mut resolved = PathBuf::new();

    match components.peek() {
        Some(Component::Prefix(prefix)) => {
            resolved.push(prefix.as_os_str());
            match components.next().and_then(|_| components.next()) {
                Some(Component::RootDir) => resolved.push(std::path::MAIN_SEPARATOR_STR),
                _ => return Err(OutputPathResolutionError::DriveRelativePath),
            }
        }
        Some(Component::RootDir) => {
            components.next();
            #[cfg(windows)]
            {
                let prefix = path_drive_prefix(base)
                    .ok_or(OutputPathResolutionError::WorkingDirectoryUnavailable)?;
                resolved.push(prefix);
            }
            resolved.push(std::path::MAIN_SEPARATOR_STR);
        }
        Some(Component::CurDir)
        | Some(Component::Normal(_))
        | Some(Component::ParentDir)
        | None => {
            resolved = base
                .canonicalize()
                .map_err(|_| OutputPathResolutionError::WorkingDirectoryUnavailable)?;
        }
    }

    let mut pending: Vec<std::ffi::OsString> = Vec::new();
    for component in components {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                pending.push(name.to_os_string());
                let candidate = pending_suffix(&resolved, &pending);
                if let Ok(canonical) = candidate.canonicalize() {
                    resolved = canonical;
                    pending.clear();
                }
            }
            Component::ParentDir => {
                if pending.pop().is_none() && !resolved.pop() {
                    // `..` above the filesystem root stays at the root.
                }
            }
            Component::Prefix(_) | Component::RootDir => return Ok(None),
        }
    }
    Ok(Some(pending_suffix(&resolved, &pending)))
}

fn pending_suffix(resolved: &Path, pending: &[std::ffi::OsString]) -> PathBuf {
    let mut out = resolved.to_path_buf();
    for component in pending {
        out.push(component);
    }
    out
}

/// Rejects `out_path` when it effectively names the input.
///
/// This early, creation-free check catches output paths whose `.`/`..`
/// components resolve to the input file — such as `new/../document.qd` or
/// `a/b/../../document.qd` — before any output directory is created, so a
/// rejected build never leaves empty intermediate directories behind.
/// Symlinks are resolved in component order (a `..` after a symlink moves to
/// the symlink target's parent), so distinct outputs are never rejected as
/// false positives. It is not the authoritative check: the canonicalized
/// same-file check immediately before the write remains the final guard.
fn reject_lexically_colliding_output(input: &Path, out_path: &Path) -> anyhow::Result<()> {
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let effective = match resolve_effective_output_without_creation(out_path, &base) {
        Ok(Some(effective)) => effective,
        // Pathological path that cannot be compared; the canonicalized
        // same-file check before the write still protects the input.
        Ok(None) => return Ok(()),
        Err(error) => return Err(anyhow::Error::new(error)),
    };

    // The input always exists (it was canonicalized while loading), so its
    // canonical form is the reference the effective output path is compared
    // against.
    let input_canonical = input.canonicalize().unwrap_or_else(|_| input.to_path_buf());
    if same_file_name(
        Some(effective.as_os_str()),
        Some(input_canonical.as_os_str()),
    ) {
        anyhow::bail!(
            "refusing to overwrite the input source file: input '{}' maps to output '{}'",
            input.display(),
            out_path.display()
        );
    }
    Ok(())
}

/// Maximum number of distinct candidate names tried for a temporary output
/// file before failing.
const MAX_TEMP_CANDIDATES: usize = 32;

/// Monotonic counter backing temporary file candidate names.
///
/// Only used to make candidate names vary across calls within a process;
/// uniqueness of a name is never relied upon for correctness. The
/// authoritative collision guard is `create_new(true)` in
/// [`write_output_atomically`].
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Returns candidate names for the temporary output file next to `out_path`.
///
/// Each name embeds the output file name, the process id, and a sequence
/// number drawn from the supplied counter:
/// `.scribium.{name}.{pid}.{seq:x}.tmp`. The leading dot keeps the file
/// hidden from plain directory listings. The first candidate is always
/// tried first; collisions found by `create_new(true)` move on to the next
/// candidate. Purely a naming scheme — uniqueness of a name is never relied
/// upon for correctness, so `start` may be any value.
fn temp_candidate_names(parent: &Path, out_path: &Path, start: u64) -> Vec<PathBuf> {
    let name = out_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let pid = std::process::id();
    (0..MAX_TEMP_CANDIDATES as u64)
        .map(|i| parent.join(format!(".scribium.{}.{}.{:x}.tmp", name, pid, start + i)))
        .collect()
}

/// Copies the permission bits of an existing output file to the replacement
/// temporary file.
///
/// Without this, replacing an existing output would silently change its mode
/// from e.g. `0640` to the temporary file's `0600`. When no output exists yet
/// the temporary file keeps its creation mode (`0666 & !umask`, see
/// [`write_output_atomically`]). Unix only; Windows has no Unix mode
/// semantics.
#[cfg(unix)]
fn preserve_existing_output_mode(out_path: &Path, tmp_path: &Path) -> std::io::Result<()> {
    match fs::metadata(out_path) {
        Ok(meta) => fs::set_permissions(tmp_path, meta.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Writes `content` to `out_path` without leaving partial output behind.
///
/// The content is written to a uniquely named temporary file inside the
/// output directory, flushed and synced, then renamed over `out_path`. On
/// any error return the temporary file is removed and any previous output
/// is left untouched; an existing output is replaced without ever being
/// truncated in place. The output parent directory must already exist —
/// call [`resolve_output_path`] first.
///
/// Permissions (Unix): the temporary file is created with
/// [`fs::OpenOptions`] plus `create_new(true)`, which applies the same
/// default mode as `std::fs::write` (`0666 & !umask`). When an output file
/// already exists, its permission bits are applied to the replacement
/// first, so re-running a build never silently changes an existing output
/// mode (e.g. from `0640` to the temporary file's `0600`).
///
/// Scope: the rename guarantees that concurrent readers never observe
/// partial content (`rename(2)` on Unix, `MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING` on Windows, whose symlink replacement
/// semantics differ). This is *not* a crash-durability guarantee: the
/// output directory is not fsynced, so power loss may not preserve the
/// newest file, and a hard process kill (SIGKILL, power loss) can leave the
/// temporary file behind — normal error-return paths clean it up.
fn write_output_atomically(out_path: &Path, content: &[u8]) -> anyhow::Result<()> {
    write_output_atomically_with(out_path, content, &TEMP_SEQUENCE)
}

/// Implementation of [`write_output_atomically`] with an explicit candidate
/// sequence counter.
///
/// The counter is advanced per attempt batch; passing a dedicated counter
/// keeps the candidate names deterministic in tests.
fn write_output_atomically_with(
    out_path: &Path,
    content: &[u8],
    sequence: &AtomicU64,
) -> anyhow::Result<()> {
    let parent = out_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let start = sequence.fetch_add(MAX_TEMP_CANDIDATES as u64, Ordering::Relaxed);

    // Create the temporary file exclusively. `create_new(true)` fails when
    // the candidate path already exists (as a regular file, directory,
    // symlink, or anything else), which is the authoritative collision
    // guard; the candidate names merely vary the attempt. On `AlreadyExists`
    // the next candidate is tried, up to a bounded number of attempts.
    let mut tmp_path: Option<PathBuf> = None;
    let mut tmp: Option<fs::File> = None;
    let mut last_conflict: Option<std::io::Error> = None;
    for candidate in temp_candidate_names(parent, out_path, start) {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                tmp_path = Some(candidate);
                tmp = Some(file);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_conflict = Some(e);
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "cannot create temporary file in {}: {}",
                    parent.display(),
                    e
                ));
            }
        }
    }
    let tmp_path = tmp_path.ok_or_else(|| {
        anyhow::anyhow!(
            "cannot create temporary file in {}: all {} candidate names are taken (last conflict: {})",
            parent.display(),
            MAX_TEMP_CANDIDATES,
            last_conflict
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default()
        )
    })?;
    let mut tmp = tmp.expect("tmp set iff tmp_path set");

    let prepared = (|| -> std::io::Result<()> {
        tmp.write_all(content)?;
        tmp.flush()?;
        tmp.sync_all()?;
        #[cfg(unix)]
        preserve_existing_output_mode(out_path, &tmp_path)?;
        Ok(())
    })();
    if let Err(e) = prepared {
        let _ = fs::remove_file(&tmp_path);
        return Err(anyhow::anyhow!(
            "cannot write {}: {}",
            out_path.display(),
            e
        ));
    }
    drop(tmp);

    if let Err(e) = fs::rename(&tmp_path, out_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(anyhow::anyhow!(
            "cannot write {}: {}",
            out_path.display(),
            e
        ));
    }
    Ok(())
}

/// Bails when `output` refers to the same file as `input`.
fn ensure_distinct_output(input: &Path, output: &Path) -> anyhow::Result<()> {
    if same_file_paths(input, output) {
        anyhow::bail!(
            "refusing to overwrite the input source file: input '{}' maps to output '{}'",
            input.display(),
            output.display()
        );
    }
    Ok(())
}

/// Returns whether two paths refer to the same file.
///
/// When the output already exists, real file identity is compared via
/// `same-file` (device/inode on Unix, file index on Windows): this detects
/// hard links and symlinks that alias the input, whatever the path spelling.
/// When the output does not exist, the parent directory of each path is
/// canonicalized (the input parent always exists) and the file names are
/// compared, which resolves `.`/`..`/relative forms without requiring the
/// output to exist. A dangling symlink is resolved manually so that a link
/// pointing at the input is still detected.
fn same_file_paths(a: &Path, b: &Path) -> bool {
    // Output exists: compare actual file identity.
    if b.exists() {
        return same_file::is_same_file(a, b).unwrap_or(false);
    }
    // A dangling symlink still creates a directory entry; writing through it
    // would create the link target. Resolve the link and compare against the
    // input before falling back to path comparison.
    if let Ok(meta) = fs::symlink_metadata(b) {
        if meta.file_type().is_symlink() {
            if let Ok(target) = fs::read_link(b) {
                let resolved = if target.is_absolute() {
                    target
                } else {
                    b.parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."))
                        .join(target)
                };
                return same_file::is_same_file(a, &resolved).unwrap_or(false);
            }
        }
    }
    // Output does not exist: normalize the parent directories and compare the
    // file names.
    match (canonical_parent(a), canonical_parent(b)) {
        (Some(parent_a), Some(parent_b)) => {
            parent_a == parent_b && same_file_name(a.file_name(), b.file_name())
        }
        _ => false,
    }
}

/// Compares file names for the not-yet-existing output case.
///
/// Windows filesystems are case-insensitive, so two names differing only in
/// case would still collide there; other platforms compare byte-exact.
#[cfg(windows)]
fn same_file_name(a: Option<&std::ffi::OsStr>, b: Option<&std::ffi::OsStr>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy()),
        _ => false,
    }
}

#[cfg(not(windows))]
fn same_file_name(a: Option<&std::ffi::OsStr>, b: Option<&std::ffi::OsStr>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Canonicalizes the parent directory of `path`, treating an empty parent as
/// the current directory. Returns `None` when the parent cannot be resolved.
fn canonical_parent(path: &Path) -> Option<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent.canonicalize().ok()
}

/// Execute the `check` command: validate input without producing output.
pub fn check(input: &str) -> anyhow::Result<()> {
    let input = Path::new(input);
    let loaded = load_single_file_project(input)?;
    let result = compile_project(&loaded.project)?;

    for diag in &result.diagnostics {
        eprintln!("{:?}", diag);
    }

    ensure_no_errors(&result.diagnostics)?;

    Ok(())
}

/// Execute the `inspect` command: show intermediate representation(s).
pub fn inspect(input: &str, emit: &str) -> anyhow::Result<()> {
    let input = Path::new(input);
    let loaded = load_single_file_project(input)?;
    let result = compile_project(&loaded.project)?;

    // Fail on error diagnostics
    ensure_no_errors(&result.diagnostics)?;

    match emit {
        "typst" => {
            let typst_code = scribium_typst::lowering::lower_to_typst_code(&result.ir);
            println!("{}", typst_code);
        }
        "ir" => {
            let json =
                serde_json::to_string_pretty(&result.ir).map_err(|e| anyhow::anyhow!("{}", e))?;
            println!("{}", json);
        }
        "ast" | "semantic" | "source-map" => {
            println!("[{} emit not yet implemented]", emit);
        }
        _ => anyhow::bail!("unknown emit target: {}", emit),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Test-only variant of [`super::build`] that keeps the pre-`--typst-path`
    /// three-argument shape for the many typst-output tests. PDF tests use
    /// [`super::build`] directly so they can pass a fake executable path.
    fn build(input: &str, formats: &[String], output: Option<&Path>) -> anyhow::Result<()> {
        super::build(input, formats, output, Path::new("typst"))
    }

    #[test]
    #[cfg(unix)]
    fn symlink_input_preserves_logical_output_path() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let link_dir = dir.path().join("link_dir");
        fs::create_dir(&link_dir).unwrap();

        // Create real file inside project root
        let real_file = link_dir.join("real.qd");
        fs::write(&real_file, "---\ntitle: Symlink Test\n---\n\n# Hello\n").unwrap();

        // Create symlink inside project root pointing to file inside project root
        let link_file = link_dir.join("link.qd");
        symlink(&real_file, &link_file).unwrap();

        // Build through CLI using the symlink path
        let result = build(&link_file.to_string_lossy(), &["typst".to_string()], None);
        assert!(result.is_ok(), "Build failed: {:?}", result);

        // Verify VirtualProject entry is logical path
        let loaded = load_single_file_project(&link_file).unwrap();
        assert_eq!(loaded.project.entry().as_str(), "link.qd");

        // Verify source store entry
        let entry = loaded.project.entry();
        let source_id = loaded
            .project
            .sources()
            .get_id(entry)
            .expect("entry source must exist");

        assert_eq!(
            loaded.project.sources().path_by_id(source_id).unwrap(),
            entry
        );

        // Output should be at link_dir/link.typ (logical path)
        let expected_output = default_typst_output_path(&link_file);
        assert!(
            expected_output.exists(),
            "output file should exist at logical path: {:?}",
            expected_output
        );

        // Verify content
        let content = fs::read_to_string(&expected_output).unwrap();
        assert!(
            content.contains("Title: Symlink Test"),
            "content was: {}",
            content
        );
        assert!(content.contains("= Hello"), "content was: {}", content);
    }

    #[test]
    #[cfg(unix)]
    fn symlink_outside_project_root_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let link_dir = dir.path().join("link_dir");
        let external_dir = dir.path().join("external");
        fs::create_dir(&link_dir).unwrap();
        fs::create_dir(&external_dir).unwrap();

        // Create real file outside project root
        let real_file = external_dir.join("real.qd");
        fs::write(&real_file, "---\ntitle: Symlink Test\n---\n\n# Hello\n").unwrap();

        // Create symlink inside project root pointing outside
        let link_file = link_dir.join("link.qd");
        symlink(&real_file, &link_file).unwrap();

        // Build through CLI using the symlink path - should fail
        let result = build(&link_file.to_string_lossy(), &["typst".to_string()], None);
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("symlink escape"));
        assert!(error.contains("outside project root"));

        // Ensure no output file was created
        let unexpected_output = default_typst_output_path(&link_file);
        assert!(!unexpected_output.exists());
    }

    #[test]
    fn native_loader_supplies_nested_resource_builtins_to_virtual_project() {
        let dir = tempdir().unwrap();
        let docs = dir.path().join("docs");
        let partials = docs.join("partials");
        let data = partials.join("data");
        fs::create_dir_all(&data).unwrap();
        let input = docs.join("main.qd");
        fs::write(&input, ".include {partials/a.qd}\n").unwrap();
        fs::write(partials.join("a.qd"), ".read {data/value.txt}\n").unwrap();
        fs::write(data.join("value.txt"), "from nested resource\n").unwrap();

        let result = build(&input.to_string_lossy(), &["typst".to_string()], None);
        assert!(result.is_ok(), "Build failed: {:?}", result);
        let output = fs::read_to_string(docs.join("main.typ")).unwrap();
        assert!(output.contains("from nested resource"), "{output}");
    }

    #[test]
    fn output_path_qd_extension() {
        let input = Path::new("document.qd");
        let out = default_typst_output_path(input);
        assert_eq!(out.to_str(), Some("document.typ"));
    }

    #[test]
    fn output_path_no_extension() {
        let input = Path::new("document");
        let out = default_typst_output_path(input);
        assert_eq!(out.to_str(), Some("document.typ"));
    }

    #[test]
    fn output_path_multiple_dots() {
        let input = Path::new("chapter.en.qd");
        let out = default_typst_output_path(input);
        assert_eq!(out.to_str(), Some("chapter.en.typ"));
    }

    #[test]
    fn output_path_hidden_file() {
        let input = Path::new(".hidden");
        let out = default_typst_output_path(input);
        assert_eq!(out.to_str(), Some(".hidden.typ"));
    }

    #[test]
    fn output_path_subdirectory() {
        let input = Path::new("src/main.qd");
        let out = default_typst_output_path(input);
        assert_eq!(out.to_str(), Some("src/main.typ"));
    }
    #[test]
    fn ensure_no_errors_fails_on_error() {
        let diagnostics = vec![scribium_core::Diagnostic {
            code: "E0001".to_string(),
            severity: scribium_core::Severity::Error,
            message: "Test error".to_string(),
            primary: None,
            secondary: vec![],
            hints: vec![],
        }];
        let result = ensure_no_errors(&diagnostics);
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("1 error"));
    }

    #[test]
    fn ensure_no_errors_passes_on_warning() {
        let diagnostics = vec![scribium_core::Diagnostic {
            code: "W0001".to_string(),
            severity: scribium_core::Severity::Warning,
            message: "Test warning".to_string(),
            primary: None,
            secondary: vec![],
            hints: vec![],
        }];
        let result = ensure_no_errors(&diagnostics);
        assert!(result.is_ok());
    }

    #[test]
    fn ensure_no_errors_passes_on_empty() {
        let diagnostics: Vec<scribium_core::Diagnostic> = vec![];
        let result = ensure_no_errors(&diagnostics);
        assert!(result.is_ok());
    }

    #[test]
    fn unimplemented_chain_callee_fails_before_typst_or_pdf_output() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("chain.qd");
        fs::write(&input, ".a::b\n").unwrap();

        assert!(check(&input.to_string_lossy()).is_err());

        let typst_result = build(&input.to_string_lossy(), &["typst".to_string()], None);
        assert!(typst_result.is_err());
        assert!(!dir.path().join("chain.typ").exists());

        let pdf_result = build(&input.to_string_lossy(), &["pdf".to_string()], None);
        assert!(pdf_result.is_err());
        assert!(!dir.path().join("chain.pdf").exists());
    }

    #[test]
    fn logical_root_for_bare_filename() {
        assert_eq!(
            logical_project_root(Path::new("document.qd")),
            PathBuf::from(".")
        );
    }

    #[test]
    fn logical_root_for_dot_prefixed_filename() {
        assert_eq!(
            logical_project_root(Path::new("./document.qd")),
            PathBuf::from(".")
        );
    }

    #[test]
    fn logical_root_for_nested_directory() {
        assert_eq!(
            logical_project_root(Path::new("docs/document.qd")),
            PathBuf::from("docs")
        );
    }

    #[test]
    fn logical_root_for_absolute_path() {
        assert_eq!(
            logical_project_root(Path::new("/abs/dir/document.qd")),
            PathBuf::from("/abs/dir")
        );
    }

    #[test]
    fn same_file_paths_relative_vs_absolute() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        // Same file: one path starts with a `.` parent, the other is absolute.
        let dotted = dir.path().join(".").join("document.qd");
        assert!(same_file_paths(&input, &dotted));
        assert!(same_file_paths(&input, &input));
        assert!(!same_file_paths(&input, &dir.path().join("document.md")));
    }

    #[test]
    fn same_file_paths_with_dotdot_components() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        // `sub/..` resolves to the input's directory, so both paths are the same file.
        let dotdot = sub.join("..").join("document.qd");
        assert!(same_file_paths(&input, &dotdot));
    }

    #[test]
    fn same_file_paths_nonexistent_output_is_never_the_input() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        // Output does not exist: same directory, different name is a different file.
        assert!(!same_file_paths(&input, &dir.path().join("out.typ")));
        // Same name in a different (existing) directory is a different file.
        let other = dir.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(!same_file_paths(&input, &other.join("document.qd")));
    }

    #[test]
    fn typ_input_is_rejected_as_unsupported_format() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.typ");
        fs::write(&input, "# Hello\n").unwrap();

        // `--output` must not matter: the extension is rejected before any
        // output path is considered.
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&input.with_extension("out.typ")),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("unsupported input extension '.typ'"),
            "error was: {}",
            error
        );
        assert!(error.contains("qd, scrib, md"), "error was: {}", error);
    }

    #[test]
    fn unknown_extension_is_rejected() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.txt");
        fs::write(&input, "# Hello\n").unwrap();

        let result = build(&input.to_string_lossy(), &["typst".to_string()], None);
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("unsupported input extension '.txt'"),
            "error was: {}",
            error
        );
    }

    #[test]
    fn extensionless_input_is_rejected() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document");
        fs::write(&input, "# Hello\n").unwrap();

        let result = build(&input.to_string_lossy(), &["typst".to_string()], None);
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("missing input extension"),
            "error was: {}",
            error
        );
        assert!(error.contains("qd, scrib, md"), "error was: {}", error);
    }

    #[test]
    fn all_supported_extensions_are_accepted() {
        for ext in ["qd", "scrib", "md"] {
            let dir = tempdir().unwrap();
            let input = dir.path().join(format!("document.{ext}"));
            fs::write(&input, "# Hello\n").unwrap();

            let result = build(&input.to_string_lossy(), &["typst".to_string()], None);
            assert!(result.is_ok(), "extension .{ext} failed: {:?}", result);

            let expected = dir.path().join("document.typ");
            assert!(expected.exists(), ".{ext} output was not written");
        }
    }

    #[test]
    fn case_insensitive_extension_is_accepted() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("DOCUMENT.QD");
        fs::write(&input, "# Hello\n").unwrap();

        let result = build(&input.to_string_lossy(), &["typst".to_string()], None);
        assert!(result.is_ok(), "Build failed: {:?}", result);
        assert!(dir.path().join("DOCUMENT.typ").exists());
    }

    #[test]
    fn check_and_inspect_apply_the_same_extension_policy() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.typ");
        fs::write(&input, "# Hello\n").unwrap();

        let check_err = check(&input.to_string_lossy()).unwrap_err().to_string();
        assert!(
            check_err.contains("unsupported input extension"),
            "{}",
            check_err
        );

        let inspect_err = inspect(&input.to_string_lossy(), "typst")
            .unwrap_err()
            .to_string();
        assert!(
            inspect_err.contains("unsupported input extension"),
            "{}",
            inspect_err
        );
    }

    #[test]
    fn explicit_output_equal_to_input_is_rejected() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&input),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "error was: {}",
            error
        );
    }

    #[test]
    fn dotdot_output_equal_to_input_is_rejected() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        let before = fs::read(&input).unwrap();

        let output = sub.join("..").join("document.qd");
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "error was: {}",
            error
        );
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
    }

    #[test]
    fn dotdot_output_through_missing_dir_is_rejected() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "original source\n").unwrap();
        let before = fs::read(&input).unwrap();

        // The `new` parent does not exist, so the path only resolves to the
        // input after the build creates the directory.
        let missing = dir.path().join("new");
        assert!(!missing.exists());
        let output = missing.join("..").join("document.qd");

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "error was: {}",
            error
        );

        // The early lexical rejection fires before any directory is created:
        // `new` must not exist, the input must survive byte-for-byte, and no
        // temporary files may be left behind. On every supported OS.
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
        assert!(!missing.exists(), "`new` must not be created");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("document.qd")],
            "no leftover entries may exist: {:?}",
            names
        );
    }

    #[test]
    fn dotdot_output_through_missing_multilevel_dirs_is_rejected() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "original source\n").unwrap();
        let before = fs::read(&input).unwrap();

        // Neither `a` nor `b` exists; only after the build creates them does
        // `a/b/../..` unwind back to the input's directory.
        let output = dir
            .path()
            .join("a")
            .join("b")
            .join("..")
            .join("..")
            .join("document.qd");
        assert!(!dir.path().join("a").exists());

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "error was: {}",
            error
        );
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );

        // The early lexical rejection fires before any directory is created:
        // neither `a` nor `a/b` may exist, and no temporary files may be
        // left behind. On every supported OS.
        assert!(!dir.path().join("a").exists(), "`a` must not be created");
        assert!(
            !dir.path().join("a").join("b").exists(),
            "`a/b` must not be created"
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("document.qd")],
            "no leftover entries may exist: {:?}",
            names
        );
    }

    #[test]
    fn nested_output_through_missing_dirs_is_written() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        let output = dir.path().join("dist").join("nested").join("document.typ");
        assert!(!output.exists());
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_ok(), "Build failed: {:?}", result);
        assert!(output.exists(), "expected output {:?} to exist", output);
        let content = fs::read_to_string(&output).unwrap();
        assert!(content.contains("Hello"), "content was: {}", content);
    }

    #[test]
    #[cfg(unix)]
    fn symlink_then_dotdot_to_distinct_output_is_allowed() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let project = dir.path().join("project");
        let other = dir.path().join("other").join("subdir");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&other).unwrap();
        let input = project.join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        let before = fs::read(&input).unwrap();
        let link = project.join("link");
        symlink(dir.path().join("other").join("subdir"), &link).unwrap();

        // `link/..` must move to the symlink target's parent (`other`), not
        // the lexical parent (`project`), so this output is distinct from
        // the input and must be allowed.
        let output = link.join("..").join("document.qd");
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_ok(), "Build failed: {:?}", result);
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
        assert!(
            !project.join("document.typ").exists(),
            "lexical path must not be touched"
        );
        let written = dir.path().join("other").join("document.qd");
        assert!(
            written.exists(),
            "expected output at {:?} (symlink target parent)",
            written
        );
        let content = fs::read_to_string(&written).unwrap();
        assert!(content.contains("Hello"), "content was: {}", content);
        let names: Vec<_> = fs::read_dir(dir.path().join("other"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| !n.to_string_lossy().starts_with(".scribium.")),
            "temporary files leaked: {:?}",
            names
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlink_then_dotdot_to_input_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let other = dir.path().join("other").join("subdir");
        fs::create_dir_all(&other).unwrap();
        let input = dir.path().join("other").join("document.qd");
        fs::write(&input, "original source\n").unwrap();
        let before = fs::read(&input).unwrap();
        let link = project.join("link");
        symlink(dir.path().join("other").join("subdir"), &link).unwrap();

        // `link/..` resolves to `other`, so the output names the input and
        // must be rejected before anything is created.
        let output = link.join("..").join("document.qd");
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "error was: {}",
            error
        );
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
        let names: Vec<_> = fs::read_dir(dir.path().join("other"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| !n.to_string_lossy().starts_with(".scribium.")),
            "temporary files leaked: {:?}",
            names
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlink_dotdot_collision_does_not_create_missing_directories() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let other = dir.path().join("other").join("subdir");
        fs::create_dir_all(&other).unwrap();
        let input = dir.path().join("other").join("document.qd");
        fs::write(&input, "original source\n").unwrap();
        let before = fs::read(&input).unwrap();
        let link = project.join("link");
        symlink(dir.path().join("other").join("subdir"), &link).unwrap();

        // `link` → `other/subdir`; `new` does not exist and is cancelled by
        // the first `..`, then `..` moves to `other`, which is the input's
        // directory. The collision must be detected without ever creating
        // `other/subdir/new`.
        let missing = other.join("new");
        assert!(!missing.exists());
        let output = link.join("new").join("..").join("..").join("document.qd");
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "error was: {}",
            error
        );
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
        assert!(!missing.exists(), "`new` must not be created");
        let names: Vec<_> = fs::read_dir(dir.path().join("other"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| !n.to_string_lossy().starts_with(".scribium.")),
            "temporary files leaked: {:?}",
            names
        );
    }

    #[test]
    #[cfg(unix)]
    fn output_symlink_to_input_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        let before = fs::read(&input).unwrap();

        // The output path is a symlink pointing at the input file.
        let output = dir.path().join("out.typ");
        symlink(&input, &output).unwrap();

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "error was: {}",
            error
        );
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
    }

    #[test]
    fn output_hardlink_to_input_is_rejected() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        let before = fs::read(&input).unwrap();

        // The output path is a hard link to the input file (same inode).
        let output = dir.path().join("out.typ");
        fs::hard_link(&input, &output).unwrap();

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "error was: {}",
            error
        );
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
    }

    #[test]
    fn rejected_build_does_not_modify_input() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "original source\n").unwrap();
        let before = fs::read(&input).unwrap();

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&input),
        );
        assert!(result.is_err());

        let after = fs::read(&input).unwrap();
        assert_eq!(before, after, "input bytes must not change on rejection");
    }

    #[test]
    fn qd_input_defaults_to_typ_output() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        let result = build(&input.to_string_lossy(), &["typst".to_string()], None);
        assert!(result.is_ok(), "Build failed: {:?}", result);

        let expected = dir.path().join("document.typ");
        assert!(expected.exists(), "expected output {:?} to exist", expected);
    }

    #[test]
    fn nonexistent_sibling_output_is_written() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        let output = dir.path().join("out.typ");
        assert!(!output.exists());
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_ok(), "Build failed: {:?}", result);
        assert!(output.exists(), "expected output {:?} to exist", output);
    }

    #[test]
    fn single_level_output_directory_is_created() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        let out_dir = dir.path().join("out");
        let output = out_dir.join("main.typ");
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_ok(), "Build failed: {:?}", result);
        assert!(output.exists(), "expected output {:?} to exist", output);
    }

    #[test]
    fn multilevel_output_directory_is_created() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        let output = dir.path().join("a").join("b").join("c").join("main.typ");
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_ok(), "Build failed: {:?}", result);
        assert!(output.exists(), "expected output {:?} to exist", output);
    }

    #[test]
    fn output_parent_that_is_a_file_fails_without_touching_input() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "original source\n").unwrap();
        let before = fs::read(&input).unwrap();

        // `blocker` exists as a regular file, so it cannot be a parent directory.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "i am a file\n").unwrap();
        let output = blocker.join("out.typ");

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("cannot create output directory"),
            "{}",
            error
        );

        // No partial output file and no stray temporary files are left behind.
        assert!(!output.exists());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "leftover files: {:?}", names);
        assert!(names.contains(&"blocker".into()));
        assert!(names.contains(&"document.qd".into()));

        assert_eq!(fs::read(&input).unwrap(), before, "input must be unchanged");
    }

    #[test]
    fn existing_output_is_atomically_replaced_without_leftovers() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        let output = dir.path().join("out.typ");
        fs::write(&output, "stale content\n").unwrap();

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_ok(), "Build failed: {:?}", result);

        let content = fs::read_to_string(&output).unwrap();
        assert!(
            !content.contains("stale content"),
            "stale output content survived replacement"
        );
        assert!(content.contains("Hello"), "content was: {}", content);

        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "temporary files leaked: {:?}", names);
    }

    #[test]
    fn write_output_atomically_rejects_failure_without_partial_file() {
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "i am a file\n").unwrap();

        let out = blocker.join("out.typ");
        let result = write_output_atomically(&out, b"content");
        assert!(result.is_err());
        assert!(!out.exists());
        assert!(!blocker.is_dir());
    }

    #[test]
    fn temp_candidate_names_follow_the_documented_format() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("out.typ");
        let names = temp_candidate_names(dir.path(), &out, 0);
        assert_eq!(names.len(), MAX_TEMP_CANDIDATES);
        let pid = std::process::id();
        assert_eq!(
            names[0].file_name().unwrap().to_string_lossy(),
            format!(".scribium.out.typ.{pid}.0.tmp")
        );
        assert_eq!(
            names[1].file_name().unwrap().to_string_lossy(),
            format!(".scribium.out.typ.{pid}.1.tmp")
        );
        assert_eq!(
            names[names.len() - 1]
                .file_name()
                .unwrap()
                .to_string_lossy(),
            format!(
                ".scribium.out.typ.{pid}.{:x}.tmp",
                MAX_TEMP_CANDIDATES as u64 - 1
            )
        );
        assert!(
            names
                .iter()
                .map(|n| n.to_string_lossy().into_owned())
                .collect::<std::collections::HashSet<_>>()
                .len()
                == names.len()
        );
    }

    #[test]
    fn temp_write_skips_first_candidate_occupied_by_regular_file() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("out.typ");
        let seq = AtomicU64::new(0);
        let first = temp_candidate_names(dir.path(), &out, 0).remove(0);
        fs::write(&first, "pre-existing blocker\n").unwrap();

        let result = write_output_atomically_with(&out, b"final output", &seq);
        assert!(result.is_ok(), "write failed: {:?}", result);
        assert_eq!(
            fs::read_to_string(&first).unwrap(),
            "pre-existing blocker\n",
            "a blocker file must never be modified"
        );
        assert_eq!(fs::read_to_string(&out).unwrap(), "final output");

        let temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| {
                n.to_string_lossy().starts_with(".scribium.")
                    && n.to_string_lossy() != first.file_name().unwrap().to_string_lossy()
            })
            .collect();
        assert!(
            temps.is_empty(),
            "no temporary files beyond the pre-existing blocker: {:?}",
            temps
        );
    }

    #[test]
    #[cfg(unix)]
    fn temp_write_skips_symlink_first_candidate() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let out = dir.path().join("out.typ");
        let seq = AtomicU64::new(0);
        let target = dir.path().join("precious.txt");
        fs::write(&target, "precious content\n").unwrap();
        let first = temp_candidate_names(dir.path(), &out, 0).remove(0);
        symlink(&target, &first).unwrap();

        let result = write_output_atomically_with(&out, b"final output", &seq);
        assert!(result.is_ok(), "write failed: {:?}", result);
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "precious content\n",
            "the symlink target must never be modified"
        );
        assert!(
            fs::symlink_metadata(&first)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the colliding symlink must be left in place"
        );
        assert_eq!(fs::read_to_string(&out).unwrap(), "final output");
    }

    #[test]
    fn temp_write_fails_cleanly_when_all_candidates_conflict() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("out.typ");
        let seq = AtomicU64::new(0);
        let mut blockers = Vec::new();
        for (i, path) in temp_candidate_names(dir.path(), &out, 0)
            .into_iter()
            .enumerate()
        {
            fs::write(&path, format!("blocker {i}\n")).unwrap();
            blockers.push((i, path));
        }

        let result = write_output_atomically_with(&out, b"final output", &seq);
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains(&format!(
                "all {MAX_TEMP_CANDIDATES} candidate names are taken"
            )),
            "error was: {}",
            error
        );
        assert!(!out.exists(), "no output may be created");
        for (i, path) in &blockers {
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                format!("blocker {i}\n"),
                "blocker files must never be deleted or modified"
            );
        }
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), blockers.len());
    }

    #[test]
    fn temp_write_leaves_no_artifacts_after_success_and_failure() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("out.typ");

        let ok = write_output_atomically(&out, b"ok");
        assert!(ok.is_ok(), "write failed: {:?}", ok);
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "i am a file\n").unwrap();
        let bad = blocker.join("out.typ");
        assert!(write_output_atomically(&bad, b"x").is_err());

        let temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().starts_with(".scribium."))
            .collect();
        assert!(temps.is_empty(), "temporary files leaked: {:?}", temps);
    }

    #[test]
    #[cfg(unix)]
    fn existing_output_mode_is_preserved_on_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        let output = dir.path().join("out.typ");
        fs::write(&output, "stale\n").unwrap();
        fs::set_permissions(&output, fs::Permissions::from_mode(0o640)).unwrap();

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_ok(), "Build failed: {:?}", result);

        let mode = fs::metadata(&output).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o640,
            "replacing an existing output must keep its permission mode"
        );
    }

    #[test]
    #[cfg(unix)]
    fn new_output_file_matches_fs_write_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        // Reference: the mode `std::fs::write` produces in this directory
        // under the current umask (`0666 & !umask`).
        let reference = dir.path().join("reference.txt");
        fs::write(&reference, "x\n").unwrap();

        let output = dir.path().join("out.typ");
        assert!(!output.exists());
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_ok(), "Build failed: {:?}", result);

        let out_mode = fs::metadata(&output).unwrap().permissions().mode() & 0o7777;
        let ref_mode = fs::metadata(&reference).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            out_mode, ref_mode,
            "a new output must be created with the same mode as fs::write"
        );
    }

    #[test]
    #[cfg(unix)]
    fn successful_build_leaves_no_temp_artifacts_in_output_dir() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        // Build to a fresh directory, then reject: no `.scribium.*.tmp`
        // files may be left in the output directory.
        let missing = dir.path().join("gen");
        let output = missing.join("nested").join("document.typ");
        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_ok(), "Build failed: {:?}", result);

        for entry in fs::read_dir(&missing).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.starts_with(".scribium."),
                "temporary file leaked: {}",
                name
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn case_variant_output_path_is_rejected_on_windows() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        let before = fs::read(&input).unwrap();

        // Windows filesystems are case-insensitive: `DOCUMENT.qd` and
        // `document.qd` name the same file.
        let variant = dir.path().join("DOCUMENT.qd");
        assert!(same_file_paths(&input, &variant));

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&variant),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "{}",
            error
        );
        assert_eq!(fs::read(&input).unwrap(), before);
    }

    #[cfg(windows)]
    fn same_drive_tempdir(prefix: &str) -> tempfile::TempDir {
        let cwd = std::env::current_dir().expect("current directory must be available");
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(&cwd)
            .expect("same-drive temporary directory must be created")
    }

    #[test]
    #[cfg(windows)]
    fn root_relative_output_colliding_with_input_is_rejected() {
        // Root-relative paths anchor at the current directory's drive, so the
        // input must live on that same drive. The temp dir may be on another
        // drive on CI (the workspace is on `D:` while `%TEMP%` is on `C:`),
        // so create a working directory under the crate's current directory.
        let dir = same_drive_tempdir(".root-relative-collision-test-");
        let input = dir.path().join("document.qd");
        fs::write(&input, "original source\n").unwrap();
        let before = fs::read(&input).unwrap();

        // Build a root-relative output path (no drive prefix) from the
        // working directory's components, inserting a missing intermediate
        // directory and a `..`: `\A\scribium\scribium\<dir>\new\..\document.qd`.
        let components: Vec<_> = input.components().skip(2).collect();
        let mut output = PathBuf::from("\\");
        for (i, component) in components.iter().enumerate() {
            if i + 1 == components.len() {
                output.push("new");
                output.push("..");
            }
            match component {
                Component::Normal(name) => output.push(name),
                _ => panic!("cwd path must be a plain drive-absolute path"),
            }
        }

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "error was: {}",
            error
        );
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
        assert!(
            !dir.path().join("new").exists(),
            "intermediate directory must not be created"
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| !n.to_string_lossy().starts_with(".scribium.")),
            "temporary files leaked: {:?}",
            names
        );
    }

    #[test]
    #[cfg(windows)]
    fn root_relative_output_to_distinct_file_is_written() {
        let dir = same_drive_tempdir(".root-relative-distinct-test-");
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        let before = fs::read(&input).unwrap();

        // `\A\scribium\scribium\<dir>\out.typ` — root-relative, no drive prefix.
        let mut output = PathBuf::from("\\");
        for component in input.components().skip(2) {
            match component {
                Component::Normal(name) => output.push(name),
                _ => panic!("cwd path must be a plain drive-absolute path"),
            }
        }
        output.pop();
        output.push("out.typ");

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        if let Err(error) = &result {
            panic!("build failed: {}", error);
        }
        let written = fs::read(dir.path().join("out.typ")).unwrap();
        let text = String::from_utf8(written).unwrap();
        assert!(text.contains("Hello"), "output content was: {:?}", text);
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
    }

    #[test]
    #[cfg(windows)]
    fn drive_relative_output_is_rejected() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "original source\n").unwrap();
        let before = fs::read(&input).unwrap();

        // `C:out\document.typ` — drive-relative, depends on the per-drive
        // current-directory state, and must be rejected explicitly.
        let prefix = match input.components().next() {
            Some(Component::Prefix(prefix)) => prefix.as_os_str().to_os_string(),
            _ => panic!("tempdir path must have a drive prefix"),
        };
        let mut output = PathBuf::from(prefix);
        output.push("out");
        output.push("document.typ");

        let result = build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            Some(&output),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("drive-relative output paths are not supported"),
            "error was: {}",
            error
        );
        assert!(
            error.contains("'C:\\...'"),
            "error should suggest an absolute path: {}",
            error
        );
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| n == &std::ffi::OsString::from("document.qd")),
            "no directory or temporary file may be created: {:?}",
            names
        );
    }

    /// Serializes write-then-spawn of the fake Typst executables.
    ///
    /// Cargo runs tests as threads in one process. Linux `execve(2)` returns
    /// `ETXTBSY` ("Text file busy") when a file is executed while any task —
    /// including a child forked by a parallel test's `Command::spawn` — still
    /// holds it open for writing, which races the freshly written fake scripts
    /// under CI load. macOS and Windows do not enforce this at exec time.
    static FAKE_TYPST_SPAWN_LOCK: Mutex<()> = Mutex::new(());

    /// Writes a fake Typst executable into `dir` whose `compile` invocation
    /// writes `pdf_body` to the output PDF argument and exits successfully.
    /// Returns the executable's path; the tests never need a real Typst
    /// install to exercise the CLI's PDF plumbing.
    fn write_fake_typst(dir: &std::path::Path, pdf_body: &str) -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = format!(
                "#!/bin/sh\nif [ \"$1\" = \"compile\" ]; then\n  if [ \"$2\" = \"--root\" ]; then output=\"$5\"; else output=\"$3\"; fi\n  printf '%s' '{}' > \"$output\"\n  exit 0\nfi\nprintf '%s\\n' 'typst fake 0.15.1'\n",
                pdf_body
            );
            let path = dir.join("fake_typst");
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
        #[cfg(windows)]
        {
            // Write the pdf body to a secondary file and `copy /B` it into
            // place, so `%` in PDF content is never interpreted by `echo`.
            let body_path = dir.join("fake_body.bin");
            fs::write(&body_path, pdf_body.as_bytes()).unwrap();
            let script = format!(
                "@echo off\nif \"%1\"==\"compile\" (\n  if \"%2\"==\"--root\" (\n    copy /B \"{}\" \"%~5\" >nul\n  ) else (\n    copy /B \"{}\" \"%~3\" >nul\n  )\n  exit /b 0\n)\necho typst fake 0.15.1\n",
                body_path.display(),
                body_path.display()
            );
            let path = dir.join("fake_typst.cmd");
            fs::write(&path, script).unwrap();
            path
        }
    }

    /// Like [`write_fake_typst`] but fails the `compile` invocation: it
    /// writes `stderr_body` to stderr and exits non-zero without producing a
    /// PDF file. Unix-only for the same reason as the fake executable itself
    /// (Windows `CreateProcess` cannot spawn `.cmd`/`.bat`).
    #[cfg(unix)]
    fn write_failing_fake_typst(dir: &std::path::Path, stderr_body: &str) -> PathBuf {
        {
            use std::os::unix::fs::PermissionsExt;
            let script = format!(
                "#!/bin/sh\nif [ \"$1\" = \"compile\" ]; then\n  printf '%s\\n' '{}' >&2\n  exit 1\nfi\nprintf '%s\\n' 'typst fake 0.15.1'\n",
                stderr_body
            );
            let path = dir.join("failing_typst");
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    #[test]
    fn typst_format_does_not_invoke_the_typst_executable() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        // Point --typst-path at a path that does not exist: a typst-only
        // build must never spawn a subprocess, so this must succeed.
        let result = super::build(
            &input.to_string_lossy(),
            &["typst".to_string()],
            None,
            Path::new("/nonexistent/typst"),
        );
        assert!(result.is_ok(), "typst-only build failed: {:?}", result);
        assert!(dir.path().join("document.typ").exists());
    }

    #[cfg(unix)]
    #[test]
    fn pdf_build_respects_custom_typst_path() {
        let dir = tempdir().unwrap();
        let _spawn_guard = FAKE_TYPST_SPAWN_LOCK.lock().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        let fake = write_fake_typst(dir.path(), "%PDF-1.7 fake");

        let result = super::build(&input.to_string_lossy(), &["pdf".to_string()], None, &fake);
        assert!(result.is_ok(), "pdf build failed: {:?}", result);
        let output = dir.path().join("document.pdf");
        assert!(output.exists(), "pdf output must exist");
        let pdf = fs::read(&output).unwrap();
        assert!(pdf.starts_with(b"%PDF-"), "pdf header was: {:?}", pdf);
        assert_eq!(pdf, b"%PDF-1.7 fake");
    }

    #[test]
    fn pdf_build_with_missing_executable_fails_cleanly() {
        let dir = tempdir().unwrap();
        let _spawn_guard = FAKE_TYPST_SPAWN_LOCK.lock().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();

        let result = super::build(
            &input.to_string_lossy(),
            &["pdf".to_string()],
            None,
            Path::new("/nonexistent/typst"),
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("Typst executable not found"),
            "error was: {}",
            error
        );
        assert!(
            !dir.path().join("document.pdf").exists(),
            "no pdf may be written when the executable is missing"
        );
    }

    #[test]
    fn unsupported_raw_html_diagnostic_blocks_pdf_before_backend_execution() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.md");
        fs::write(&input, "<div>\n**not Markdown**\n</div>\n").unwrap();

        let result = super::build(
            &input.to_string_lossy(),
            &["pdf".to_string()],
            None,
            Path::new("/nonexistent/typst"),
        );
        let error = result.expect_err("unsupported HTML must reject PDF output");
        assert!(error.to_string().contains("found 1 error(s)"));
        assert!(
            !dir.path().join("document.pdf").exists(),
            "unsupported HTML must not produce a PDF"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pdf_compilation_failure_leaves_no_output_and_surfaces_diagnostic() {
        let dir = tempdir().unwrap();
        let _spawn_guard = FAKE_TYPST_SPAWN_LOCK.lock().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "original source\n").unwrap();
        let before = fs::read(&input).unwrap();
        let fake = write_failing_fake_typst(dir.path(), "fake typst error: bad syntax");

        let result = super::build(&input.to_string_lossy(), &["pdf".to_string()], None, &fake);
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("fake typst error"),
            "compiler diagnostic must be surfaced, error was: {}",
            error
        );
        assert!(
            !dir.path().join("document.pdf").exists(),
            "no pdf file may be written on compilation failure"
        );
        assert_eq!(
            fs::read(&input).unwrap(),
            before,
            "input bytes must not change"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pdf_invalid_header_is_rejected_without_writing_output() {
        let dir = tempdir().unwrap();
        let _spawn_guard = FAKE_TYPST_SPAWN_LOCK.lock().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        // The fake exits 0 but returns a non-PDF body.
        let fake = write_fake_typst(dir.path(), "garbage not a pdf");

        let result = super::build(&input.to_string_lossy(), &["pdf".to_string()], None, &fake);
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("invalid PDF output: missing %PDF- header"),
            "error was: {}",
            error
        );
        assert!(
            !dir.path().join("document.pdf").exists(),
            "no pdf file may be written for an invalid PDF header"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pdf_and_typst_formats_produce_both_outputs() {
        let dir = tempdir().unwrap();
        let _spawn_guard = FAKE_TYPST_SPAWN_LOCK.lock().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        let fake = write_fake_typst(dir.path(), "%PDF-1.7 fake");

        let result = super::build(
            &input.to_string_lossy(),
            &["typst".to_string(), "pdf".to_string()],
            None,
            &fake,
        );
        assert!(result.is_ok(), "multi-format build failed: {:?}", result);
        let typst = dir.path().join("document.typ");
        let pdf = dir.path().join("document.pdf");
        assert!(typst.exists(), ".typ output must exist");
        assert!(pdf.exists(), ".pdf output must exist");
        assert!(fs::read(&pdf).unwrap().starts_with(b"%PDF-"));
    }

    #[cfg(unix)]
    #[test]
    fn pdf_explicit_output_is_respected() {
        let dir = tempdir().unwrap();
        let _spawn_guard = FAKE_TYPST_SPAWN_LOCK.lock().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        let fake = write_fake_typst(dir.path(), "%PDF-1.7 fake");
        let output = dir.path().join("custom").join("out.pdf");

        let result = super::build(
            &input.to_string_lossy(),
            &["pdf".to_string()],
            Some(&output),
            &fake,
        );
        assert!(result.is_ok(), "pdf build failed: {:?}", result);
        assert!(output.exists(), "explicit pdf output must exist");
        assert!(fs::read(&output).unwrap().starts_with(b"%PDF-"));
    }

    #[test]
    fn pdf_output_equal_to_input_is_rejected() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "original source\n").unwrap();
        let fake = write_fake_typst(dir.path(), "%PDF-1.7 fake");

        let result = super::build(
            &input.to_string_lossy(),
            &["pdf".to_string()],
            Some(&input),
            &fake,
        );
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refusing to overwrite the input source file"),
            "error was: {}",
            error
        );
    }

    #[test]
    fn unsupported_formats_fail_for_pdf_too() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("document.qd");
        fs::write(&input, "# Hello\n").unwrap();
        let fake = write_fake_typst(dir.path(), "%PDF-1.7 fake");

        for format in ["html", "svg", "png"] {
            let result = super::build(&input.to_string_lossy(), &[format.to_string()], None, &fake);
            assert!(result.is_err());
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("not yet implemented"),
                "format {} error was: {}",
                format,
                error
            );
        }
    }
}
