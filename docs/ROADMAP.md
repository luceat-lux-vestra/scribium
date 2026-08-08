# Roadmap — Scribium

Status markers: `Not started` | `In progress` | `Completed` | `Deferred`

## M0 — Foundation

**Objective:** Establish legal boundaries, technology choices, and repository structure.

| Item                           | Status       |
|--------------------------------|--------------|
| Repository bootstrap           | Completed    |
| LICENSE/NOTICE                 | Completed    |
| Product documentation          | Completed    |
| Name due diligence             | Completed    |
| Typst integration spike        | Completed    |
| Markdown parser spike          | Completed    |
| ADR 0001–0010                  | Completed    |
| GitHub templates/CI            | Completed    |
| WASM build in CI               | Completed    |
| VirtualProject abstraction     | Completed    |

**Architecture constraint:** `scribium-core` + `scribium-typst` (lowering)
MUST compile for `wasm32-unknown-unknown`. CI verifies this from M0.
Core uses `VirtualProject` for all I/O — no filesystem access.

**Dependencies:** None

## M0.5 — Upstream Compatibility Infrastructure

**Objective:** Detect Quarkdown upstream drift before compatibility implementation continues.

| Item                                | Status       |
|-------------------------------------|--------------|
| Machine-readable upstream baseline  | Completed    |
| Stable release observer             | Completed    |
| Drift issue automation              | Completed    |
| Conformance corpus foundation       | Completed    |

**Dependencies:** M0

## M1 — Quarkdown-Compatible Vertical Slice

**Status:** Completed

**Objective:** First end-to-end `.qd → Typst → PDF` pipeline.

Acceptance: dot-prefixed calls, positional/named/body arguments, basic conditional,
front matter, deterministic output.

> **Front Matter scope:** currently a flat line-based `key: value` format only.
> Delimiters and metadata lines must start at column 0; indented keys reject
> the block. Nested objects, arrays, and block strings (full YAML) are deferred
> to a later milestone and tracked separately.

## M2 — Quarkdown Core Language + Markdown MVP

**Status:** In progress

**Objective:** Production-ready Quarkdown core subset and Markdown baseline.
v0.1.0 release.

| Item                                | Status       |
|-------------------------------------|--------------|
| Document-scope variable evaluation  | Completed    |
| Remaining M2 features               | In progress  |

## M3 — Programmable Documents

**Objective:** Components, data loading, iteration, resource limits.

## M4 — Developer Experience

**Objective:** Watch mode, inspect commands, source maps, structured diagnostics.

## M5 — Quarkdown Compatibility Expansion

**Objective:** Expand compatibility coverage and conformance corpus.

## M6 — Library API, LSP, WASM Bindings

**Objective:** Embedding, editor integration, `scribium-wasm` bindings crate.

WASM compilation is an M0 architecture constraint (core + lowering).
M6 delivers the `scribium-wasm` bindings crate and WASM CI coverage.

## M7 — Hardening

**Objective:** Fuzzing, benchmarks, security audit, 1.0 release.

---

## Explicitly Deferred Work

- Browser-side Typst compilation (scribium-typst-web, gate behind feasibility)
- LSP server (deferred to M6, core API must stabilize first)
- Package registry (not planned)
- Web editor / SaaS (not planned)
- JavaScript plugin runtime (not planned)
- Full Quarkdown 100% compatibility (not a goal)