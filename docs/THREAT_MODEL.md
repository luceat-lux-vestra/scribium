# Threat Model — Scribium

## Scope

This document covers security threats to Scribium and its users when
processing untrusted document input. It does not cover Typst compiler
vulnerabilities (report to Typst GmbH).

## Threat Matrix

| #  | Threat                      | Asset                | Attacker          | Boundary       | Mitigation                          | Residual Risk |
|----|-----------------------------|----------------------|-------------------|----------------|-------------------------------------|---------------|
| T1 | Path traversal via include  | Filesystem           | Malicious doc     | Include/read   | VirtualPath resolution; Typst `--root`; canonicalized mirror scope | Low        |
| T2 | Symlink escape              | Filesystem           | Malicious doc     | Include/read   | Final canonical target must remain inside the explicit root; mirror copies no symlinks | Low |
| T3 | Unrestricted include        | Filesystem           | Malicious doc     | Include/read   | Active include-stack cycle detection; root scope; future configured depth limit | Low |
| T4 | Malicious image or font     | Process              | Malicious doc     | Asset loading  | Typst compiler handles              | Low (inherited) |
| T5 | Decompression bomb          | Memory               | Malicious doc     | Asset loading  | Max file size, format validation    | Medium        |
| T6 | Deep nesting (AST)          | Memory               | Malicious doc     | Parser         | Max nesting depth                   | Low           |
| T7 | Exponential expansion       | CPU / Memory         | Malicious doc     | Evaluator      | Per-operation materialization bound; aggregate output remains deferred | Medium |
| T8 | Infinite recursion          | CPU / Stack          | Malicious doc     | Evaluator      | Scoped active evaluator-depth bound | Low           |
| T9 | Large loop count            | CPU                  | Malicious doc     | Evaluator      | Per-operation materialized-element bound | Low           |
| T10 | Hostile regex               | CPU                  | Malicious doc     | Evaluator      | No user-provided regex in core      | Low           |
| T11 | Environment leakage         | Secrets              | Malicious doc     | Evaluator      | Environment access disabled by def  | Low           |
| T12 | Arbitrary shell execution   | System               | Malicious doc     | Evaluator      | No shell execution                  | None (blocked) |
| T13 | Network access              | Network              | Malicious doc     | Evaluator      | Network denied by default           | None (blocked) |
| T14 | Typst package resolution    | Network / Filesystem | Malicious doc     | Typst backend  | Package scope policy, no auto-install | Medium        |
| T15 | Untrusted template          | Filesystem           | Malicious doc     | Include        | Same path validation as T1          | Low           |
| T16 | Generated file overwrite    | Filesystem           | Malicious doc     | CLI / backend  | Atomic output, overwrite protection | Low           |
| T17 | Terminal escape injection   | Display              | Malicious doc     | Diagnostics    | Diagnostic output sanitization      | Low           |
| T18 | Generated Typst injection   | Output               | Malicious doc     | Lowering       | Typst escape proper escaping        | Low           |
| T19 | Generated Typst reads outside root | Filesystem       | Malicious doc     | Typst backend  | Explicit root, temporary mirror, Typst `--root`, fail-closed traversal | Low |

## Default Security Policy

```
network:          denied
shell:            denied
environment:      denied
filesystem:       explicit project-root scoped; temporary mirror for Typst reads
symlink escape:   denied (final canonical target outside root is rejected)
absolute include: denied by default
```

## VirtualPath Security Boundary

Core uses `VirtualPath` (logical path strings) exclusively. The native CLI
adapter translates VirtualPath → `PathBuf` at the boundary, applying:

- Root-scoping (no path leaves project root)
- Canonicalization (no `..` segments)
- Symlink resolution (which VirtualPath itself does not model)

A WASM frontend does not perform this translation at all — there is no
filesystem to traverse. This eliminates the T1/T2 attack surface entirely
for browser targets. Resource-backed evaluator builtins use the same
source-relative logical paths in WASM and native builds; native loading is the
only stage that reads the host tree.

For the native Typst subprocess path, the explicit source root is copied into
an isolated temporary mirror. The generated entry file is created in that
mirror, while the PDF is written to a separate temporary output location.
The source tree is therefore a read-only resource context and never a write
location for generated Typst, PDF, or temporary metadata. Typst is invoked with
`--root` pointing at the mirror, so parent traversal and absolute host paths
cannot escape the staged project boundary. A symlink is allowed only when its
final canonical target remains inside the explicit source root; both file and
directory symlink escapes are rejected.

## Configurable Resource Limits

| Limit                    | Default        | Max        |
|--------------------------|----------------|------------|
| Max source size          | 10 MB          | 100 MB     |
| Max include depth        | 16             | 64         |
| Max AST nodes            | 100,000        | 1,000,000  |
| Max materialized elements per evaluator operation | 1,000,000 | typed `usize` |
| Max active evaluator depth | 256 | typed `usize` |
| Max generated Typst size | 10 MB          | 100 MB     |
| Max asset size           | 5 MB           | 50 MB      |
| Compile timeout          | 60s            | 300s       |
| Max number of files      | 1,000          | 10,000     |

All limit violations produce a clear diagnostic message.

## Responsible Disclosure

See `SECURITY.md` for vulnerability reporting procedures.
