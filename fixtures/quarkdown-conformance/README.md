# Quarkdown Conformance Corpus

This directory contains independently authored conformance test cases for Scribium's
Quarkdown compatibility implementation.

## Structure

```
fixtures/quarkdown-conformance/
├── README.md              # This file
├── cases/                 # Individual test cases
│   ├── <case-id>/
│   │   ├── case.toml      # Case metadata
│   │   ├── input.qd       # Independently authored Quarkdown input
│   │   └── expected/      # Expected outputs (added incrementally)
│   │       ├── ast.json   # Expected AST (when implemented)
│   │       └── typst.typ  # Expected Typst output (when implemented)
```

## Case Metadata Schema (`case.toml`)

```toml
# Required fields
id = "call-positional-basic"
feature = "positional-arguments"
compatibility_level = "Parsed"
specification_source = "quarkdown-function-call-syntax"
description = "Basic positional argument call"

# Optional fields
# known_divergence = "Description of known divergence"  # omit if none
```

### Fields

| Field | Description |
|-------|-------------|
| `id` | Unique identifier (kebab-case), used for test naming |
| `feature` | Feature name from the compatibility matrix (e.g., `dot-prefixed-call`, `positional-arguments`, `named-arguments`, `indented-body`, `conditionals`, `variables`) |
| `compatibility_level` | One of: `Unsupported`, `Parsed`, `Semantically supported`, `Output-equivalent`, `Known divergence` |
| `specification_source` | Short key referencing the specification source in `SPEC_SOURCES.md` |
| `description` | Human-readable description of what this case tests (required) |
| `known_divergence` | Omitted if none, or a description of a documented divergence |

## Adding New Cases

1. Create a new directory under `cases/` with the case ID as name
2. Write `case.toml` with the metadata
3. Write `input.qd` with an independently authored Quarkdown input
4. **Do not** copy inputs from Quarkdown test suites or documentation examples
5. Run the conformance harness to verify the case executes

## Clean-Room Policy

All test inputs in this corpus are **independently authored** by Scribium contributors
based on public specification documentation only. No inputs are copied from:
- Quarkdown source code
- Quarkdown test fixtures
- Quarkdown documentation examples (verbatim)
- quarkdown-wasm or related repositories

See `docs/legal/CLEAN_ROOM_POLICY.md` for the full policy.