# Quarkdown Compatibility Specification

## Status

- **Specification version:** 0.3 (reference baseline v2.5.0)
- **Reference upstream:** Quarkdown v2.5.0
- **Compatibility model:** documented feature subset
- **Full Quarkdown compatibility:** not claimed

## Scope

This document defines Scribium's Quarkdown-compatible syntax and semantics.
Each feature records its specification source, compatibility level, and known
divergences.

Scribium does **not** claim full Quarkdown v2.5.0 compatibility. The
compatibility contract is the **documented subset** defined by this document:
only rows marked `Implemented`, at the stated compatibility level, and covered
by Scribium conformance tests (see [Conformance Evidence](#conformance-evidence))
are part of the current compatibility contract. `SPEC_SOURCES.md` records the
upstream specification provenance for those claims. The Feature Matrix also
lists `Planned` and `Not implemented` rows; those are tracking entries and
constitute **no** compatibility claim. Features that Quarkdown v2.5.0
documents but Scribium does not implement are not bugs; they are outside the
contract until implemented and recorded in this matrix (see ADR 0012).

The Quarkdown function-call grammar is implemented clean-room from the public
documentation, notably *"Syntax of a function call"* on the Quarkdown wiki.
No Quarkdown source code is copied or translated. See `SPEC_SOURCES.md` for
provenance records.

## Feature Matrix

| Feature                        | Syntax                           | Compatibility            | Status           |
|--------------------------------|----------------------------------|--------------------------|------------------|
| Dot-prefixed call              | `.note`                          | Parsed                   | Implemented      |
| Implicit positional refs       | `.1`, `.2`, ...                  | Parsed                   | Implemented      |
| Positional arguments           | `.range {1} {10}`                | Parsed                   | Implemented      |
| Named arguments                | `.panel width:{320}`             | Parsed                   | Implemented      |
| Mixed positional/named         | `.panel {Intro} width:{320}`     | Parsed                   | Implemented      |
| Indented body argument         | `.panel {x}` + indent            | Parsed                   | Implemented      |
| Nested calls                   | `.outer {.inner {x}}`            | Parsed                   | Implemented      |
| Inline (mid-paragraph) call    | `see .note {x}`                  | Parsed                   | Implemented      |
| Tight-call boundaries          | word adjacency rejected          | Parsed                   | Implemented      |
| Malformed-call diagnostics     | `E2001`, `E2002`, `E2003`        | Error                    | Implemented      |
| Variables                      | `.var {name} {value}`, `.name`, `.name {value}`, `.if {.name}` | Semantically supported | Implemented      |
| Conditionals                   | `.if {cond}` / `.ifnot {cond}` | Semantically supported | Implemented      |
| Iteration                      | —                                | —                        | Planned          |
| Functions/components            | —                                | —                        | Planned          |
| Include/read                   | —                                | —                        | Planned          |
| Metadata                       | —                                | —                        | Planned          |
| Row/column/grid                | —                                | —                        | Planned          |
| Semantic evaluation            | `.if`/`.ifnot` + variables       | Partial / In progress    | Planned          |
| Call chaining (`::`)           | `.a {x}::b {y}`                  | Not implemented          | Planned          |
| Line continuation (`\`)        | `\` at end of line               | Not implemented          | Planned          |
| Tight / brace-wrapped calls    | `.x` wrapped in braces at adjacency | Not implemented       | Planned          |
| Multi-line arguments           | `{.…}` parsing spans lines        | Not implemented (E2xxx today) | Planned          |
| `.json` data loading           | `.json {path}` (new in v2.5.0)   | Not implemented          | Planned          |
| `.markdown` / `.llmstxt`       | (new in v2.5.0)                  | Not implemented          | Planned          |

`Implemented` rows are covered by unit and golden tests (see
[Conformance Evidence](#conformance-evidence)). `Planned` means the syntax is
not implemented yet, in whole or in part: it has no documented `Unsupported`
diagnostic and must not be assumed to work.

## Conformance Evidence

Each `Implemented` row is backed by at least one Scribium conformance test.
The table maps every `Implemented` feature to the test(s) that verify it;
`quarkdown/parser.rs` refers to
`crates/scribium-core/src/syntax/quarkdown/parser.rs` and `markdown/parser.rs`
to `crates/scribium-core/src/syntax/markdown/parser.rs`. A single test may
cover multiple features. This table is the implementation-evidence
counterpart of the upstream provenance recorded in `SPEC_SOURCES.md`; the two
are kept separate on purpose.

| Feature                         | Evidence (unit tests) |
|---------------------------------|------------------------|
| Dot-prefixed call               | `quarkdown/parser.rs::parse_call_no_args`, `quarkdown/parser.rs::parse_call_underscore_name`, `quarkdown/parser.rs::parse_call_hyphen_name`, `quarkdown/parser.rs::empty_and_plain_text_are_not_calls`, `markdown/parser.rs::block_call_no_arguments`, `markdown/parser.rs::valid_calls_produce_no_diagnostics` |
| Implicit positional refs        | `quarkdown/parser.rs::implicit_positional_references`, `quarkdown/parser.rs::implicit_reference_boundary_stops_at_word_characters`, `quarkdown/parser.rs::implicit_reference_survives_symbol_boundaries`, `markdown/parser.rs::implicit_reference_call_at_block_level`, `markdown/parser.rs::implicit_reference_inline_boundaries` |
| Positional arguments            | `quarkdown/parser.rs::parse_call_positional_scalar`, `quarkdown/parser.rs::parse_call_positional_string`, `quarkdown/parser.rs::parse_call_boolean_args`, `quarkdown/parser.rs::multiple_args_with_various_whitespace`, `markdown/parser.rs::block_call_positional_args` |
| Named arguments                 | `quarkdown/parser.rs::parse_call_named_args`, `markdown/parser.rs::block_call_named_args` |
| Mixed positional/named          | `quarkdown/parser.rs::parse_mixed_args`, `markdown/parser.rs::block_call_mixed_args` |
| Indented body argument          | `markdown/parser.rs::block_call_with_indented_body`, `markdown/parser.rs::block_call_body_span_covers_indented_lines`, `markdown/parser.rs::block_body_may_contain_markdown_and_nested_calls`, `markdown/parser.rs::body_requires_minimum_indentation`, `markdown/parser.rs::body_single_tab_counts_as_body`, `markdown/parser.rs::body_stops_at_less_indented_line` |
| Nested calls                    | `quarkdown/parser.rs::parse_nested_call_in_argument`, `markdown/parser.rs::nested_call_inside_argument`, `markdown/parser.rs::block_body_may_contain_markdown_and_nested_calls` |
| Inline (mid-paragraph) call     | `markdown/parser.rs::inline_call_in_sentence`, `markdown/parser.rs::call_with_trailing_text_is_inline_call`, `markdown/parser.rs::inline_call_at_line_start_continues_paragraph`, `markdown/parser.rs::isolated_call_line_still_starts_block` |
| Tight-call boundaries           | `quarkdown/parser.rs::tight_word_adjacency_makes_call_ordinary_text`, `quarkdown/parser.rs::symbols_are_valid_call_boundaries`, `quarkdown/parser.rs::implicit_reference_does_not_consume_arguments`, `markdown/parser.rs::tight_call_boundary_rejects_trailing_word`, `markdown/parser.rs::tight_call_hyphen_boundaries_are_valid`, `markdown/parser.rs::unicode_word_characters_are_tight_adjacency`, `markdown/parser.rs::inline_call_does_not_parse_in_numbers` |
| Malformed-call diagnostics      | `quarkdown/parser.rs::positional_after_named_is_rejected` (`E2001`), `quarkdown/parser.rs::named_argument_without_braces_is_error` (`E2002`), `quarkdown/parser.rs::unclosed_argument_is_error` (`E2003`), `markdown/parser.rs::malformed_calls_produce_structured_diagnostics`, `markdown/parser.rs::malformed_calls_do_not_panic_and_fall_back_to_paragraph` |
| Conditionals                   | `evaluator.rs::if_true_keeps_block_body`, `evaluator.rs::if_false_drops_block_body`, `evaluator.rs::ifnot_true_drops_and_ifnot_false_keeps`, `evaluator.rs::boolean_identifiers_yes_no_true_false_case_insensitive`, `evaluator.rs::missing_condition_reports_e3001_and_drops`, `evaluator.rs::unresolvable_condition_reports_diagnostic`, `evaluator.rs::nested_if_inside_block_body_is_evaluated`, `evaluator.rs::content_value_second_argument_replaces_call`, `evaluator.rs::scalar_second_argument_becomes_text`, `evaluator.rs::inline_if_replaces_call_with_inline_body_or_content`, `evaluator.rs::inline_if_false_drops_call`, `evaluator.rs::inline_call_scalar_second_argument_becomes_text`, `evaluator.rs::non_conditional_calls_are_preserved_with_evaluated_bodies`, `evaluator.rs::named_condition_argument_works`, `evaluator.rs::named_condition_false_drops_body`, `evaluator.rs::named_condition_ifnot_inverts`, `evaluator.rs::named_condition_identifier_yes_no`, `evaluator.rs::named_body_argument_works`, `evaluator.rs::named_body_scalar_argument_works`, `evaluator.rs::block_body_priority_over_named_body`, `evaluator.rs::inline_named_condition_works`, `evaluator.rs::inline_named_body_works`, `evaluator.rs::named_condition_unresolvable_reports_e3001`, `lib.rs::compile_evaluates_if_true`, `lib.rs::compile_evaluates_if_false`, `lib.rs::compile_evaluates_ifnot`, `lib.rs::compile_evaluates_nested_if`, `lib.rs::compile_reports_e3001_for_unresolvable_condition`, `lib.rs::compile_evaluates_named_condition_true`, `lib.rs::compile_evaluates_named_condition_false`, `lib.rs::compile_evaluates_named_condition_yes_no`, `lib.rs::compile_evaluates_named_body`, `lib.rs::compile_evaluates_named_condition_and_body`, `lib.rs::compile_inline_named_condition`, `typst::conditional_evaluation_before_lowering` |
| Variables                      | `evaluator.rs::var_scalar_definition_and_reference`, `evaluator.rs::var_boolean_reference_in_conditional`, `evaluator.rs::var_false_boolean_drops_conditional`, `evaluator.rs::var_ifnot_with_variable`, `evaluator.rs::var_explicit_reassignment`, `evaluator.rs::var_variable_name_reassignment`, `evaluator.rs::var_reassignment_produces_no_output`, `evaluator.rs::var_inline_use`, `evaluator.rs::var_block_variable`, `evaluator.rs::var_conditional_declaration_execution_order`, `evaluator.rs::var_unknown_call_preserved`, `evaluator.rs::var_malformed_declaration_reports_e3002`, `evaluator.rs::var_nested_evaluation_in_block_variable`, `evaluator.rs::var_evaluation_immutable_and_deterministic`, `lib.rs::compile_variable_declaration_and_reference`, `lib.rs::compile_variable_boolean_in_conditional`, `lib.rs::compile_variable_false_conditional`, `lib.rs::compile_variable_ifnot`, `lib.rs::compile_variable_explicit_reassignment`, `lib.rs::compile_variable_name_reassignment`, `lib.rs::compile_variable_inline_use`, `lib.rs::compile_variable_block_variable`, `lib.rs::compile_variable_conditional_declaration`, `lib.rs::compile_variable_unknown_preserved`, `lib.rs::compile_variable_malformed_reports_e3002`, `lib.rs::compile_variable_nested_in_block`, `lib.rs::compile_variable_immutable_and_deterministic` |

## Compatibility Levels

- **Unsupported:** Produces explicit `E8xxx` diagnostic (used only by the
  compatibility-profile diagnostics; see `compatibility/diagnostics.rs`)
- **Error:** Produces an explicit parse diagnostic (`E2xxx`) at the call site
- **Parsed:** Accepted syntactically; behavior may be undefined or rejected
- **Semantically supported:** Scribium semantics match documented behavior
- **Output-equivalent:** Typst output matches reference for tested inputs
- **Known divergence:** Deliberate behavioral difference with documented
  rationale

Function calls are currently **Parsed**: `.name`, positional arguments
`{arg}`, named arguments `name:{arg}`, nested calls, and indented block
bodies are parsed into the Scribium AST/IR. **Conditional evaluation
(`.if` / `.ifnot`) with boolean literals and variable references
(`.if {.name}`) is implemented**. Full semantic evaluation (functions,
iteration, components) remains the next milestone (see `docs/SYNTAX.md` and
`docs/ROADMAP.md`). Note that a feature which currently fails to parse
(e.g. `E2xxx` syntax errors on some input forms) is still labeled by its
documented support level in the matrix — an input-level parse error is
not an `Unsupported` marker.

### Tight-call boundaries

A call requires a boundary before and after it: whitespace, a symbol
(including `-`), or the start/end of the line. A call directly adjacent to a
word character — any Unicode letter or digit, plus `_` — is not recognized and
the whole construct stays ordinary text. Examples:

- `.note {x}` is a call; `.note {x}B` and `한.note {x}` are not (both
  Unicode and ASCII letters count as word characters).
- `-.note` and `.note-` are valid calls: `-` is a symbol, not a word
  character.

The new-in-Quarkdown brace-wrapped form (`H{.text {2}}O`), which lifts the
boundary requirement, is a documented v2.5.0 behavior but is **not
implemented** here; the inner call parses, but the wrapping braces are kept
as literal text.

### v2.5.0 additions outside the contract

Quarkdown has documented features that are part of the v2.5.0 baseline but are
outside Scribium's current contract. These are listed in the Feature Matrix as
`Planned` and are **not** claimed as compatible. They do not produce `E8xxx`
diagnostics today and their current behavior is undefined for the purposes of
this contract; examples: line continuation (`\` at EOL), `::` chaining, tight
brace-wrapped calls, multi-line arguments spanning raw lines, and the new
v2.5.0 builtins (data loading, `.markdown`).

## Specification Record Format

Each implemented compatibility feature records its public documentation
source, an independently authored input example, and the observed behavior.

```yaml
feature: dot-prefixed-call
specification_source: |
  Quarkdown wiki, "Syntax of a function call":
  https://quarkdown.com/wiki/syntax-of-a-function-call/ (v2.5.0 badge)
independently_authored_input: |
  .heading level:{1}
      Title
  .strong {bold text}
observed_reference_behavior: |
  Dot-prefixed names form function calls; each argument is wrapped in
  curly braces; named arguments use name:{value}; indented lines after
  a block call form its body. The current v2.5.0 documentation describes the
  same basic dot-prefixed, brace-argument model on which Scribium's existing
  parser subset is based.
scribium_behavior: |
  Parses dot calls, positional/named arguments, nested calls, and
  indented bodies into the shared DirectiveCall AST.
compatibility_level: Parsed
known_divergence: null
```

The `independently_authored_input` is written from the public syntax
specification only; it is not copied from Quarkdown sources, examples, or
tests (clean-room policy, see `docs/adr/0007-quarkdown-compatibility-scope-and-clean-room-process.md`).

## Provenance

The call grammar was derived from the public documentation *"Syntax of a
function call"* (wiki, badged `2.5.0`, accessed 2026-08-08). The current
v2.5.0 documentation describes the same basic dot-prefixed, brace-argument
model on which Scribium's parser subset is based. Scribium's previous
compatibility baseline was 0.9.x, but no claim is made that the upstream
grammar was verified to be identical across every version in between.
`SPEC_SOURCES.md` documents the source list, per-source version badges, and
accessed dates.

## Known Divergences

- (None yet for the implemented call-syntax subset)
- Scope note: only the features listed in the matrix above are implemented
  and claimed. Rows marked **Planned** are *not* implemented; anything
  documented in Quarkdown but absent from this matrix must not be assumed to
  work in Scribium.
- **Block variable evaluation timing:** Scribium evaluates block variable
  content at declaration time (source order). The cited Quarkdown public
  documentation does not explicitly specify evaluation timing for stored
  block content. This behavior may be refined if upstream semantics are
  clarified. See `docs/SYNTAX.md` for details.

## Upstream Baseline Management

Scribium's Quarkdown compatibility is tracked against a **supported baseline** version,
which is distinct from the **latest observed upstream release**.

### Baseline vs. Observed

| Concept | Description | Authority |
|---------|-------------|-----------|
| **Supported baseline** | The Quarkdown version Scribium claims compatibility with. Recorded in `upstream.toml`. | Human-reviewed PR only |
| **Latest observed** | The latest stable Quarkdown release detected by the automated observer. | Automated daily check |

The automated observer (`.github/workflows/upstream-quarkdown.yml`) runs daily and compares the latest stable release against the supported baseline:

- If they match → `current` status, no action
- If they differ → `drift` status, a GitHub Issue is created with a checklist for compatibility investigation

**Crucially:** The observer **never** updates the supported baseline. A baseline change requires:
1. Review of permitted public specification changes
2. Independently authored conformance cases
3. Black-box observations where necessary
4. Implementation of required compatibility changes
5. Full conformance suite pass
6. Documentation of known divergences
7. Human-reviewed PR updating `upstream.toml` and this compatibility matrix

See `docs/adr/0013-upstream-compatibility-observation-and-baseline-promotion.md` for the full decision record.

### Why the Compatibility Matrix Does Not Auto-Update

A new upstream release may add, change, or remove features. The compatibility matrix reflects only what Scribium has **independently verified** through:
- Public specification review
- Independently authored conformance tests
- Black-box behavioral observation (where permitted)

Automatic updates would conflate "upstream released something" with "Scribium implements it correctly" — which violates the clean-room policy and the principle that compatibility claims require evidence.

## Features Outside the Contract

The following are not part of the documented subset and are not claimed:

- Quarkdown interactive slide runtime
- Quarkdown internal plugin ABI
- Quarkdown-specific CSS themes
- Quarkdown HTML post-processing
- Quarkdown line click interactivity

New v2.5.0 builtins (data loading via `.json`, `.markdown`, `.llmstxt`,
stdlib `foreach`/iterables) are tracked as `Planned` above; they do not belong
here because none produces an `E8xxx` diagnostic today. As features are
implemented, they move from this section into the Feature Matrix.