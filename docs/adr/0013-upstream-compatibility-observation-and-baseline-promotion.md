# ADR-0013: Upstream Compatibility Observation and Baseline Promotion

- **Status:** Accepted
- **Date:** 2026-08-08
- **Owners:** Scribium maintainers
- **Related issues:** M0.5 Upstream Observer Foundation
- **Supersedes:**
- **Superseded by:**

## Context

Scribium is a Quarkdown-compatible compiler. As upstream Quarkdown evolves, Scribium must track compatible changes without relying on manual monitoring. The compatibility baseline (currently v2.5.0) defines what Scribium claims to support. New upstream releases must be detected, assessed, and deliberately adopted — never automatically.

## Decision Drivers

- **Reliability:** Compatibility claims must not drift from reality
- **Clean-room integrity:** Observer must not inspect implementation source
- **Human authority:** Baseline changes require deliberate review with evidence
- **Automation safety:** Observer never modifies product source or configuration
- **Transparency:** Drift detection must produce auditable records

## Considered Options

### Option 1: Manual monitoring (rejected)

Developers periodically check Quarkdown releases and update baseline.
- Pros: Simple, no tooling needed
- Cons: Error-prone, easy to forget, no audit trail, scales poorly

### Option 2: Automated observer with automatic baseline bump (rejected)

Observer detects new release, updates `upstream.toml`, opens PR.
- Pros: Fully automated
- Cons: Violates clean-room (baseline change without evidence), conflates detection with adoption, no human review gate

### Option 3: Automated observer with drift detection and manual baseline promotion (chosen)

Observer detects drift, creates deduplicated Issue with checklist. Baseline change only via human-reviewed PR with conformance evidence.
- Pros: Clear separation of concerns, audit trail, clean-room compliant, human authority preserved
- Cons: Requires tooling, manual step for baseline update

## Decision

Adopt Option 3. The system comprises:

1. **Machine-readable manifest** (`docs/compatibility/quarkdown/upstream.toml`)
   - Declares `supported_baseline` (e.g., `v2.5.0`)
   - Does NOT store `latest_observed` — no bot commits

2. **Testable drift detector** (`tools/upstream-watch/scribium-upstream-watch`)
   - Pure comparison logic: `observed_tag` vs `supported_baseline`
   - Outputs structured JSON with `status: current | drift` and deterministic `issue_key`
   - No network access; receives observed metadata as CLI args
   - Exit code 0 for both current and drift; non-zero only for actual errors

3. **Scheduled GitHub Actions workflow** (`.github/workflows/upstream-quarkdown.yml`)
   - Daily cron + `workflow_dispatch` with `observed_tag` override and `dry_run`
   - Fetches latest stable release metadata from GitHub API (`tag_name`, `html_url`)
   - Invokes watcher tool, evaluates result
   - On drift: checks for existing Issue by deterministic marker `<!-- scribium-upstream-drift:quarkdown:vX.Y.Z -->`
   - Creates Issue with checklist and clean-room warning if not exists
   - Minimal permissions: `contents: read`, `issues: write`

4. **Conformance corpus foundation** (`fixtures/quarkdown-conformance/`)
   - Independently authored test cases with metadata
   - Harness in `scribium-test-support` loads and verifies cases
   - Seed cases for already-implemented features

5. **Documentation**
   - ROADMAP: M0.5 for infrastructure, M5 redefined as coverage expansion
   - Compatibility README: baseline vs. observed distinction documented
   - Baseline promotion procedure recorded

## Consequences

### Positive

- Drift detected automatically within 24h of stable release
- No baseline change without human-reviewed conformance evidence
- Clean-room boundary enforced: observer reads only release metadata
- Deduplicated Issues prevent spam
- Dry-run support for safe testing
- Deterministic `issue_key` enables tracking

### Negative

- Baseline update still requires manual PR (by design)
- Observer does not analyze changelogs or specification diffs (future work)
- Conformance corpus requires ongoing maintenance

### Risks

- **Observer fires on pre-release/RC:** Mitigated by using GitHub "latest stable release" API
- **Issue created for non-breaking patch release:** Checklist prompts review; human decides if action needed
- **Marker-based deduplication fails if Issue body edited:** Marker is HTML comment, unlikely to be removed; search covers open/closed

## Validation Plan

- Unit tests for watcher: current/drift/invalid manifest/empty inputs
- Deterministic `issue_key` generation verified
- Conformance harness loads and verifies seed cases
- Baseline consistency test validates cross-document alignment
- Workflow syntax and permissions validated
- Dry-run manual trigger produces expected output without creating Issue

## Rollback or Migration Plan

- Remove workflow file to disable observer
- Remove `tools/upstream-watch` from workspace members
- Delete `upstream.toml` and conformance corpus if feature abandoned
- No database or external state to migrate

## References

- `docs/compatibility/quarkdown/upstream.toml`
- `tools/upstream-watch/`
- `.github/workflows/upstream-quarkdown.yml`
- `fixtures/quarkdown-conformance/`
- `crates/scribium-test-support/src/lib.rs` (conformance harness)
- `docs/ROADMAP.md` (M0.5, M5)
- `docs/compatibility/quarkdown/README.md` (Baseline vs. Observed section)
- `docs/legal/CLEAN_ROOM_POLICY.md`
- `docs/adr/0007-quarkdown-compatibility-scope-and-clean-room-process.md`
- `docs/adr/0012-quarkdown-compatibility-baseline.md`