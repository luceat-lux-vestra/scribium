# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Evaluator resource budgets (R10):** `scribium-engine` now applies typed,
  deterministic per-operation materialization and per-compilation evaluator
  depth limits. Closed-range cardinality is checked before conversion,
  reservation, or iteration; recursive/callback frames use scoped accounting;
  failures report source-backed `E3005` diagnostics. Defaults are 1,000,000
  materialized elements per operation and 256 active evaluator frames.

- **Quarkdown project-backed resource builtins (M2):** `.read`, `.json`, and
  `.include` now operate through the in-memory `VirtualProject`, with
  source-relative nested resolution, recursive typed JSON values, active-stack
  include-cycle detection, deterministic missing/boundary/UTF-8 diagnostics,
  and identical in-memory/WASM semantics. `.markdown` preserves the upstream
  raw native Markdown-content contract; `.llmstxt` remains deferred because it
  is not present in the reviewed Quarkdown v2.5.1 standard builtin surface.
  The native CLI loads bounded project resources before compilation and never
  exposes arbitrary host filesystem access to the evaluator.

- **Markdown local images (M2):** Inline and reference image syntax now flows
  from the Rushdown-backed AST through backend-neutral IR to Typst `#image(...)`
  and real PDF output. Relative resources resolve from the source entry
  directory inside the explicit project root; parent-relative in-root paths
  and SVG/PNG resources are covered by Typst-required integration tests.
  Image alt content and titles remain source-backed in AST/IR. Absolute paths,
  URI schemes, and remote fetching are rejected, while missing or unsupported
  local formats fail through the Typst backend.

- **Compatibility policy:** ADR-0016 establishes complete compatibility with
  the publicly documented Quarkdown language and document-observable semantics
  as the long-term target while keeping current verified claims partial and
  evidence-based. It also defines stable-release adaptation tracking, the
  separate Typst backend compatibility policy, and the engineering quality
  contract.

- **Markdown Inline Code Spans (M2)**: Inline code span parsing (`` `code` ``)
  and lowering to Typst `#raw(...)`.
  - Opening and closing backtick runs of any length (``foo` bar`` stays
    inside a double-backtick span); only a run of exactly the same length
    closes the span
  - Contents are opaque: no Markdown or Quarkdown syntax inside a code span
    is parsed (emphasis, links, and dot calls stay literal); backslashes are
    literal
  - CommonMark normalization: line endings become single spaces; content
    starting and ending with ASCII spaces (but not all-space) loses exactly
    one space on each side
  - Unmatched opening runs recover deterministically as literal text with no
    character loss and no diagnostic
  - `Inline::Code` AST node and `IrInline::Code` IR node; the evaluator
    passes code spans through unchanged (never resolves variables, never
    recurses); spans cover the delimiters
  - Typst lowering to `#raw("...")` with safe string escaping (source-map
    entries recorded); parser, IR conversion, lowering, source-map, and
    end-to-end tests
- **Markdown Inline Links (M2)**: Inline link parsing (`[text](url)`) and
  lowering to Typst `#link(...)`.
  - `Inline::Link` AST node and `IrInline::Link` IR node with the label kept
    as inline markup (emphasis, strong, Quarkdown inline calls) and the
    destination preserved as-is (no normalization, no resolution)
  - Typst lowering to `#link("destination")[label]` with safe escaping of
    `\` and `"` in destinations; source map entries for link spans
  - Deterministic subset: labels end at the first `]`, balanced parentheses
    allowed in destinations; destinations containing whitespace or control
    characters are not links (so link titles fall back to literal text);
    reference links, autolinks, and images are not supported
  - Malformed links (`[text](`, `[text]`, `[](url)`, `[text]( )`) recover
    as literal text
  - Parser, IR conversion, evaluator, lowering, and source-map tests
- **Markdown Ordered Lists (M2)**: Ordered list parsing (`1. `, `2. `, `N. `) and
  lowering to Typst with starting ordinal preservation, parentheses marker
  (`1) `, `2) `) support, and ordered/unordered nesting in either direction.
  - Ordered list AST node with `start` field in `Block::OrderedList`
  - Ordered list IR node in `IrNode::OrderedList`
  - Typst lowering to native Typst enumeration syntax; nested lists are
    indented by two spaces per level and keep their hierarchy
  - Content indentation is derived from each item's own marker width (no
    fixed 3-space rule); only the first ordinal sets the list start, so
    `3. A` then `9. B` is one list starting at 3
  - Parser tests covering basic lists, non-1 start, nested lists, mixed
    ordered/unordered nesting, source spans, and malformed prefixes
  - IR conversion and lowering tests
- **M2 Core Language**: Document-scope variable evaluation (`.var` declaration, parameterless reference, reassignment, block/content variables, conditional integration)
  - `.var {name} {value}` — scalar declarations (string, number, boolean, identifier)
  - `.var {name} {**content**}` — rich/content-valued declarations preserving strong/emphasis
  - `.var {name}\n    content` — block variables with indented body
  - `.name` — parameterless variable reference (inline and block)
  - `.name {new-value}` — variable-name reassignment (only for existing variables)
  - `.if {.name}` — boolean variable conditions with `yes`/`no`/`true`/`false` identifiers
  - Malformed `.var` declarations produce `E3002` diagnostic
  - Invalid variable names (per `normal-call-name` grammar) produce `E3002` diagnostic
  - Unknown parameterless calls preserved as function calls (no variable error inflation)
  - Evaluation context is deterministic, immutable input IR, no global mutable state
  - Unit, compile-level, and lowering regression coverage
- **Compatibility**: Variables feature matrix updated to `Implemented` / `Semantically supported`; provenance recorded for Variables, Boolean, and Syntax of a function call wiki pages
- **Milestone**: M1 marked `Completed`, M2 marked `In progress` with variable evaluation as first feature

- Repository bootstrap with Apache-2.0 licensing and provenance policy.
- Product vision defining Scribium as a Quarkdown-compatible compiler.
- Clean-room Quarkdown compatibility policy and process documentation.
- Architecture, roadmap, and non-goals documentation.
- Baseline Rust workspace with scribium-core, scribium-typst, scribium-cli, scribium-test-support.
- ADR process and initial architecture decisions.
- GitHub templates for issues and pull requests.
- CI workflow with fmt, clippy, test, and dependency checks.
- M0 Foundation milestone — clean-room policy, naming research, parser/backend spikes.
- Minimal CommonMark-compatible Markdown parser (`syntax::markdown`) with
  byte-level source spans: ATX headings, paragraphs, emphasis/strong,
  unordered lists with nesting, fenced code blocks, thematic breaks, and
  hard/line breaks. No panics on malformed input.
- Source span infrastructure: `SourceId`, `ByteSpan`, `LineColumn`, `SourceSpan`.
- Structured diagnostics with stable error codes (`Diagnostic`, `Severity`).
- Compatibility profile selection and divergence tracking.
- CLI commands: `build`, `check`, `inspect`.
- Typst backend trait (`TypstBackend`) with `SubprocessBackend` adapter skeleton.
- Typst lowering skeleton (`lower_to_typst`).
- `build` accepts a bare file name (`scribium build document.qd`), resolving
  its project root to the current directory.
- `build --output <path>` to override the generated output path.
- **PDF output via external Typst subprocess** — `scribium build --format pdf` compiles
  supported input documents (`.qd`, `.scrib`, `.md`) directly to PDF using the
  configured Typst executable. The `SubprocessBackend` implements the `TypstBackend`
  trait, invoking `typst compile` via `std::process::Command` without shell
  interpolation. Real `typst --version` detection is implemented. Typst diagnostics
  are captured and surfaced as actionable Scribium errors. Generated PDFs are
  validated for non-empty output and correct `%PDF-` header.
- `--typst-path <PATH>` selects the Typst executable used for PDF output (defaults
  to `typst` on `PATH`); a `typst`-only build never spawns a subprocess.
- Multiple output formats in a single invocation — `scribium build --format typst
  --format pdf` produces both `.typ` and `.pdf` from a single lowering pass.
- Explicit `--output` path support for PDF; collision/overwrite protection and
  atomic write semantics are preserved from the Typst output path.
- Backend unit tests covering missing executable, non-zero exit, successful
  execution, output reading, `%PDF-` header validation, and version command —
  runnable without a Typst install (fake executable fixtures).
- Backend integration tests (`crates/scribium-typst/tests/backend_integration.rs`)
  exercising the real `typst` executable; they skip with a notice when it is
  absent, and CI installs a pinned Typst version (0.15.1) explicitly and runs
  them with `SCRIBIUM_REQUIRE_TYPST=1`.
- CLI integration tests for `--format pdf`, `--format typst --format pdf`,
  custom Typst path, missing executable, compilation failure, `%PDF-` validation,
  `--output` with PDF, unsupported format rejection (HTML/SVG/PNG), and input/output
  collision checks.
- README quickstart and status table updated to reflect experimental PDF support.

- **M1 Evaluator**: minimal semantic evaluator resolving Quarkdown `.if` /
  `.ifnot` conditional constructs (boolean-literal conditions only:
  `true`/`false`/`yes`/`no` case-insensitive). Named `condition` and `body`
  arguments supported per Quarkdown function signature. Nested conditionals
  supported. Unresolvable conditions produce `E3001` evaluation diagnostic
  and deterministic false treatment.
### Fixed

- `build` with multiple formats and `--output` now returns a clear validation error
  instead of silently using the output path for only one format.
- `build` never overwrites the input source file: an output that resolves to
  the input is rejected with a clear error. Existing outputs are compared by
  file identity (device/inode on Unix, file index on Windows via `same-file`),
  so relative/absolute spellings, `.`/`..` components, symlinks, and hard
  links that alias the input are all detected; the check is repeated
  immediately before writing. Rejected builds leave the input byte-for-byte
  unchanged.
- Hard and soft line breaks previously reached the IR with a synthesized
  `0..0` source span; they now carry the actual break position (byte
  offsets), matching the span policy of every other inline node.
- Output is written atomically: the content goes to a uniquely named
  temporary file in the output directory, is flushed and synced, and is then
  renamed over the output path. The temporary file is created exclusively
  with `create_new(true)` — candidate names include the PID and an
  in-process counter, and up to 32 candidates are retried when one is
  already taken, so the write never clobbers an existing file. A failed
  build no longer leaves a partial output file or a stray temporary file on
  its error-return path, and an existing output is replaced without ever
  being truncated in place. (On Unix the rename is `rename(2)`; on Windows
  it uses `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`.) This is an
  atomic-replace guarantee, not a crash-durability guarantee: the output
  directory is not fsynced, so power loss may not preserve the newest file,
  and an abrupt kill (SIGKILL, power loss) can leave a temporary file behind.
- `build --output` whose parent directories do not exist could still resolve
  to the input file *after* the directories were created — e.g.
  `new/../document.qd` with `new` missing — and overwrite it. Parent
  directories are now created first, the effective output path is resolved
  from the real (canonicalized) parent, and the same-file check runs against
  that resolved path immediately before the atomic write. The input is
  never modified, even for `.`/`..`-containing output paths.
- Output paths whose real resolution is the input (e.g. `new/../document.qd`
  or `a/b/../../document.qd` with the intermediate directories missing) are
  now rejected *before* any directory is created, so a rejected build no
  longer leaves empty `new`/`a`/`a/b` directories behind; the pre-validation
  resolves the requested path in component order with symlinks interpreted
  as reached (a `..` after a symlink moves to the symlink target's parent),
  so distinct targets behind a symlink are accepted and only real aliases
  are rejected. The canonicalized same-file check remains the authoritative
  guard for symlink and hard-link aliases.
- Windows output path kinds are now classified explicitly: root-relative
  paths (`\out\main.typ`) are resolved from the current drive's root
  (previously they silently skipped the pre-write collision check), and
  drive-relative paths (`C:out\main.typ`) are rejected with a clear error
  suggesting an absolute or ordinary relative path, since they depend on
  the per-drive current-directory state. Resolution failures are reported
  instead of being silently ignored.
- Console test targets build on Windows (unused-import warnings only surfaced
  on non-unix platforms).

### Changed
- Historical compatibility baseline: the then-current Quarkdown evidence
  policy was referenced against **v2.5.0** (see superseded ADR-0012). The
  compatibility matrix and provenance records were updated at that time; no
  parser, evaluator, lowering, or compiler semantic behavior changed. The
  default compatibility-profile label was updated from `quarkdown-v0.9` to
  `quarkdown-v2.5`. `::` chaining, line continuation, tight/brace-wrapped
  calls, and new builtins remained unimplemented at that release.
- Supported output formats are now `typst` and `pdf`; `html`, `svg`, `png` remain
  explicitly unsupported with actionable error messages.
- CLI help text updated: `--output` documents the format-dependent default
  (`.typ` for typst, `.pdf` for pdf) and `--format` lists only the supported formats.
- Supported CLI inputs are now `.qd`, `.scrib`, and `.md`; a `.typ` input is
  rejected as an unsupported format until Typst passthrough is implemented.
  Extension matching is ASCII case-insensitive, and files without an
  extension are rejected.
- `build --output` now creates missing output parent directories (single- or
  multi-level) instead of failing when they do not exist.
- Unix output permissions: replacing an existing output keeps its permission
  bits (no silent change from e.g. `0640` to the temp file's `0600`), and
  new outputs are created with `0666 & !umask` — the same default mode
  `std::fs::write` produces. The output temporary file is now created
  directly (`fs::File::create` + `rename`) instead of via a permission-locked
  helper crate. Windows behavior is unchanged.
- Source ID allocation in `SourceStore` no longer wraps: `u32::MAX` is never
  assigned, and exhaustion is reported as `SourceStoreError::SourceIdExhausted`
  before any store mutation.
- Front matter is documented as a flat line-based `key: value` format, not
  full YAML: nested objects, arrays, and block strings are not supported.
  Keys split on the first colon; delimiters and metadata lines must start at
  column 0 (indented keys reject the block, which is preserved as regular
  Markdown); duplicate keys use last-wins semantics; user-defined metadata is
  stored in the IR in deterministic order.
- Added the `same-file` dependency for cross-platform file-identity checks.
- Windows CI previously failed to compile the `scribium` test binary due to
  unused imports on Windows-only configurations; this is resolved.
- Issue templates: fixed label formatting (`type: bug` → `type:bug`),
  added milestone dropdown to feature requests.
- Removed duplicate/obsolete GitHub labels: `bug`, `enhancement`,
  `duplicate`, `invalid`, `question`, `good first issue`, `help wanted`,
  `dependencies`.
- Updated external dependencies via `cargo update`.
- Repo management: closed #2 (bootstrap completed), extracted remaining
  M0 tasks into #11 (name due diligence) and #12 (in-process Typst
  feasibility). Assigned #4, #5, #6 to M1 Vertical Slice milestone.
