mod engine_adapter;
/// `scribium-core` — Scribium's foundational library.
///
/// Responsibilities:
/// - Composition of the Markdown/Quarkdown frontend
/// - Project-to-engine input adaptation
/// - Compiler orchestration and public facade types
/// - Document IR and shared diagnostic compatibility facades
pub mod ir;
pub mod source;
pub mod source_map;

// Compatibility facade: builtin implementation ownership lives in
// scribium-engine. Evaluator keeps a core wrapper because its legacy public
// project entry point must remain available without moving project types into
// the engine.
pub use scribium_engine::builtins;
pub mod evaluator {
    use crate::engine_adapter::VirtualProjectResourceProvider;
    use crate::{Capabilities, EvaluationLimits, VirtualProject};
    use scribium_diagnostics::Diagnostic;
    use scribium_engine::evaluator as engine_evaluator;
    use scribium_ir::IrDocument;
    use scribium_source::SourceId;

    /// Core compatibility facade for the engine evaluator.
    #[derive(Debug, Clone, Copy)]
    pub struct Evaluator {
        inner: engine_evaluator::Evaluator,
    }

    impl Default for Evaluator {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Evaluator {
        /// Creates a new evaluator.
        pub fn new() -> Self {
            Self {
                inner: engine_evaluator::Evaluator::new(),
            }
        }

        /// Creates an evaluator with explicit compatibility capabilities.
        pub fn with_capabilities(capabilities: Capabilities) -> Self {
            Self {
                inner: engine_evaluator::Evaluator::with_capabilities(capabilities),
            }
        }

        /// Creates an evaluator with explicit semantic resource limits.
        pub fn with_limits(limits: EvaluationLimits) -> Self {
            Self {
                inner: engine_evaluator::Evaluator::with_limits(limits),
            }
        }

        /// Creates an evaluator with explicit capabilities and semantic
        /// resource limits for one compilation.
        pub fn with_capabilities_and_limits(
            capabilities: Capabilities,
            limits: EvaluationLimits,
        ) -> Self {
            Self {
                inner: engine_evaluator::Evaluator::with_capabilities_and_limits(
                    capabilities,
                    limits,
                ),
            }
        }

        /// Evaluates an IR document without project-backed resources.
        pub fn evaluate(&self, document: &IrDocument) -> (IrDocument, Vec<Diagnostic>) {
            self.inner.evaluate(document)
        }

        /// Evaluates an IR document using the legacy project-backed entry
        /// point. Project access remains adapted in core and is delegated to
        /// the physical engine implementation.
        pub fn evaluate_project(
            &self,
            project: &VirtualProject,
            source_id: SourceId,
            document: &IrDocument,
        ) -> (IrDocument, Vec<Diagnostic>) {
            let resource_provider = VirtualProjectResourceProvider::new(project);
            self.inner.evaluate_with_resources(
                &resource_provider,
                source_id,
                document,
                &scribium_engine::DocumentMetadataDefaults::default(),
            )
        }

        pub(crate) fn evaluate_with_resources<R: scribium_engine::ResourceProvider>(
            &self,
            resources: &R,
            source_id: SourceId,
            document: &IrDocument,
            metadata_defaults: &scribium_engine::DocumentMetadataDefaults,
        ) -> (IrDocument, Vec<Diagnostic>) {
            self.inner
                .evaluate_with_resources(resources, source_id, document, metadata_defaults)
        }
    }
}

/// Compatibility facade for the AST-to-IR module.
///
/// The implementation is in `scribium-engine`; these small adapters preserve
/// the historical core signatures that accepted project metadata and the
/// core-private source-mode distinction.
pub mod ast_to_ir {
    use super::{engine_adapter, ProjectMetadata, SourceMode};
    use scribium_diagnostics::Diagnostic;
    use scribium_ir::IrDocument;
    use scribium_markdown::ast::Document;
    use scribium_source::SourceId;

    pub fn ast_to_ir_with_diagnostics(
        doc: &Document,
        source_id: SourceId,
        project_metadata: &ProjectMetadata,
    ) -> (IrDocument, Vec<Diagnostic>) {
        scribium_engine::ast_to_ir::ast_to_ir_with_diagnostics(
            doc,
            source_id,
            &engine_adapter::document_metadata_defaults(project_metadata),
        )
    }

    pub(crate) fn ast_to_ir_with_diagnostics_for_mode(
        doc: &Document,
        source_id: SourceId,
        project_metadata: &ProjectMetadata,
        source_mode: SourceMode,
    ) -> (IrDocument, Vec<Diagnostic>) {
        scribium_engine::ast_to_ir::ast_to_ir_with_diagnostics_for_mode(
            doc,
            source_id,
            &engine_adapter::document_metadata_defaults(project_metadata),
            match source_mode {
                SourceMode::Markdown => scribium_markdown::Mode::Markdown,
                SourceMode::Quarkdown => scribium_markdown::Mode::Quarkdown,
            },
        )
    }
}
pub use scribium_project::virtual_project;

// Compatibility facade: implementation ownership lives in scribium-compat.
pub use scribium_compat as compatibility;
// Compatibility facade: implementation ownership lives in scribium-diagnostics.
pub use scribium_diagnostics as diagnostics;
pub use scribium_diagnostics::{Diagnostic, Severity};
pub use source::*;
// Compatibility facade: implementation ownership lives in scribium-project.
pub use scribium_engine::{Capabilities, Capability, EvaluationLimits};
pub use scribium_project::{BuildError, ProjectMetadata, VirtualProject, VirtualProjectBuilder};

/// The Scribium core result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Top-level error type for the core crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Diagnostic(#[from] Diagnostic),
    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceMode {
    Markdown,
    Quarkdown,
}

fn source_mode_for_entry(entry: &scribium_project::VirtualPathBuf) -> SourceMode {
    let is_markdown = entry
        .file_name()
        .and_then(|file_name| file_name.rsplit_once('.'))
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("md"));
    if is_markdown {
        SourceMode::Markdown
    } else {
        SourceMode::Quarkdown
    }
}

/// Compile a Scribium project through the full pipeline.
///
/// Returns a `CompileResult` with the generated IR and diagnostics.
/// The entry point source and its `SourceId` come from the project's
/// `SourceStore`; no global ID generator is involved.
pub fn compile(
    project: &scribium_project::VirtualProject,
    options: &CompileOptions,
) -> CompileResult {
    compile_with_capabilities(project, options, Capabilities::compatibility_default())
}

/// Compile a Scribium project with an explicit evaluator capability set.
pub fn compile_with_capabilities(
    project: &scribium_project::VirtualProject,
    options: &CompileOptions,
    capabilities: Capabilities,
) -> CompileResult {
    let entry = project.entry();

    // Use get_with_id to get both source and SourceId atomically.
    let Some((source, source_id)) = project.sources().get_with_id(entry) else {
        return CompileResult {
            ir: ir::IrDocument {
                nodes: vec![],
                metadata: ir::IrMetadata::default(),
            },
            diagnostics: vec![Diagnostic {
                code: "E9001".to_string(),
                severity: Severity::Error,
                message: "internal error: entry source or SourceId missing in project".to_string(),
                primary: None,
                secondary: vec![],
                hints: vec![
                    "this indicates an internal VirtualProject invariant violation".to_string(),
                ],
            }],
        };
    };

    let source_mode = source_mode_for_entry(entry);
    let parsed = match source_mode {
        SourceMode::Markdown => {
            scribium_markdown::parse_with_mode(source, scribium_markdown::Mode::Markdown)
        }
        SourceMode::Quarkdown => {
            scribium_markdown::parse_with_mode(source, scribium_markdown::Mode::Quarkdown)
        }
    };
    let metadata_defaults = engine_adapter::document_metadata_defaults(project.metadata());
    let (ir, lowering_diagnostics) = ast_to_ir::ast_to_ir_with_diagnostics_for_mode(
        &parsed.document,
        source_id,
        project.metadata(),
        source_mode,
    );
    let resource_provider = engine_adapter::VirtualProjectResourceProvider::new(project);
    let (ir, evaluation_diagnostics) =
        evaluator::Evaluator::with_capabilities_and_limits(capabilities, options.evaluation_limits)
            .evaluate_with_resources(&resource_provider, source_id, &ir, &metadata_defaults);
    let mut diagnostics: Vec<Diagnostic> = parsed
        .diagnostics
        .into_iter()
        .map(|d| Diagnostic {
            code: d.code.to_string(),
            severity: Severity::Error,
            message: d.message,
            primary: Some(scribium_source::SourceSpan {
                source_id,
                start: d.span.start,
                end: d.span.end,
            }),
            secondary: Vec::new(),
            hints: Vec::new(),
        })
        .collect();
    diagnostics.extend(evaluation_diagnostics);
    diagnostics.extend(lowering_diagnostics);
    CompileResult { ir, diagnostics }
}

/// Options for the compilation pipeline.
#[derive(Debug, Clone, Default)]
pub struct CompileOptions {
    pub compatibility_profile: Option<String>,
    /// Semantic evaluator limits applied to this compilation.
    pub evaluation_limits: EvaluationLimits,
}

/// Result of compilation through the frontend.
#[derive(Debug, Clone)]
pub struct CompileResult {
    pub ir: ir::IrDocument,
    pub diagnostics: Vec<Diagnostic>,
}

#[cfg(test)]
mod tests {
    use crate::ir::{IrInline, IrNode};
    use crate::{
        CompileOptions, EvaluationLimits, Severity, SourceMode, VirtualPathBuf,
        VirtualProjectBuilder,
    };
    #[test]
    fn it_compiles_empty_document() {
        let project = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", "")
            .expect("valid path")
            .build()
            .unwrap();
        let result = super::compile(
            &project,
            &CompileOptions {
                compatibility_profile: None,
                evaluation_limits: EvaluationLimits::default(),
            },
        );
        assert!(result.ir.nodes.is_empty());
    }

    #[test]
    fn compile_options_default_uses_documented_evaluation_limits() {
        let options = CompileOptions {
            compatibility_profile: None,
            evaluation_limits: EvaluationLimits::default(),
        };
        assert!(options.compatibility_profile.is_none());
        assert_eq!(options.evaluation_limits, EvaluationLimits::default());
    }

    #[test]
    fn compile_propagates_evaluation_limits_to_the_engine() {
        let project = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", ".range {1} {3}::size\n")
            .expect("valid path")
            .build()
            .unwrap();
        let result = super::compile(
            &project,
            &CompileOptions {
                compatibility_profile: None,
                evaluation_limits: EvaluationLimits {
                    max_materialized_elements: 2,
                    max_evaluation_depth: 16,
                },
            },
        );

        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        assert_eq!(result.diagnostics[0].code, "E3005");
        assert!(result.diagnostics[0]
            .message
            .contains("materialized element limit exceeded"));
        assert_eq!(
            result.diagnostics[0]
                .primary
                .as_ref()
                .map(|span| span.source_id),
            Some(crate::SourceId(1))
        );
    }

    #[test]
    fn compile_uses_project_metadata_without_front_matter() {
        let project = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", "hello")
            .expect("valid path")
            .title("Project Title")
            .author("Project Author")
            .date("2026-01-01")
            .field("custom", "project_value")
            .build()
            .unwrap();

        let result = super::compile(&project, &CompileOptions::default());
        assert_eq!(result.ir.metadata.title, Some("Project Title".into()));
        assert_eq!(result.ir.metadata.author, Some("Project Author".into()));
        assert_eq!(result.ir.metadata.date, Some("2026-01-01".into()));
        assert_eq!(result.ir.metadata.raw.len(), 1);
        assert_eq!(
            result.ir.metadata.raw[0],
            ("custom".into(), "project_value".into())
        );
    }

    #[test]
    fn compile_front_matter_overrides_typed_project_metadata() {
        let project = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source(
                "main.qd",
                "---\ntitle: FM Title\nauthor: FM Author\ndate: 2025-12-31\n---\n\nhello",
            )
            .expect("valid path")
            .title("Project Title")
            .author("Project Author")
            .date("2026-01-01")
            .build()
            .unwrap();

        let result = super::compile(&project, &CompileOptions::default());
        // Front matter overrides project metadata
        assert_eq!(result.ir.metadata.title, Some("FM Title".into()));
        assert_eq!(result.ir.metadata.author, Some("FM Author".into()));
        assert_eq!(result.ir.metadata.date, Some("2025-12-31".into()));
    }

    #[test]
    fn compile_front_matter_overrides_custom_project_metadata() {
        let project = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", "---\ncustom: fm_value\n---\n\nhello")
            .expect("valid path")
            .field("custom", "project_value")
            .build()
            .unwrap();

        let result = super::compile(&project, &CompileOptions::default());
        assert_eq!(result.ir.metadata.raw.len(), 1);
        assert_eq!(
            result.ir.metadata.raw[0],
            ("custom".into(), "fm_value".into())
        );
    }

    #[test]
    fn compile_preserves_non_overridden_project_metadata() {
        let project = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", "---\ntitle: FM Title\n---\n\nhello")
            .expect("valid path")
            .title("Project Title")
            .author("Project Author")
            .field("custom", "project_value")
            .build()
            .unwrap();

        let result = super::compile(&project, &CompileOptions::default());
        // title overridden by front matter
        assert_eq!(result.ir.metadata.title, Some("FM Title".into()));
        // author preserved from project
        assert_eq!(result.ir.metadata.author, Some("Project Author".into()));
        // custom preserved from project (not in front matter)
        assert_eq!(result.ir.metadata.raw.len(), 1);
        assert_eq!(
            result.ir.metadata.raw[0],
            ("custom".into(), "project_value".into())
        );
    }
    #[test]
    fn known_metadata_keys_are_not_duplicated_in_raw() {
        let project = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source(
                "main.qd",
                "---\ntitle: FM Title\nauthor: FM Author\ndate: 2025-12-31\n---\n\nhello",
            )
            .expect("valid path")
            .title("Project Title")
            .author("Project Author")
            .date("2026-01-01")
            .build()
            .unwrap();

        let result = super::compile(&project, &CompileOptions::default());

        // Typed fields from front matter should be in typed fields only, not in raw
        assert_eq!(result.ir.metadata.title, Some("FM Title".into()));
        assert_eq!(result.ir.metadata.author, Some("FM Author".into()));
        assert_eq!(result.ir.metadata.date, Some("2025-12-31".into()));

        // raw should be empty (no duplicate of title/author/date)
        assert_eq!(result.ir.metadata.raw.len(), 0);
    }

    #[test]
    fn custom_metadata_order_does_not_affect_compiled_ir() {
        // Build two projects with same custom metadata but different insertion order
        let project1 = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", "hello")
            .expect("valid path")
            .field("zeta", "last")
            .field("alpha", "first")
            .field("epsilon", "middle")
            .build()
            .unwrap();

        let project2 = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", "hello")
            .expect("valid path")
            .field("epsilon", "middle")
            .field("zeta", "last")
            .field("alpha", "first")
            .build()
            .unwrap();

        let result1 = super::compile(&project1, &CompileOptions::default());
        let result2 = super::compile(&project2, &CompileOptions::default());

        // IR metadata should be identical regardless of field insertion order
        assert_eq!(result1.ir.metadata.raw, result2.ir.metadata.raw);

        // Verify sorting: should be alphabetical by key
        assert_eq!(
            result1.ir.metadata.raw,
            vec![
                ("alpha".into(), "first".into()),
                ("epsilon".into(), "middle".into()),
                ("zeta".into(), "last".into()),
            ]
        );
    }

    #[test]
    fn source_ids_are_independent_of_builder_insertion_order() {
        let project1 = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("a.qd", "content a")
            .expect("valid path")
            .add_source("b.qd", "content b")
            .expect("valid path")
            .add_source("main.qd", "main")
            .expect("valid path")
            .build()
            .unwrap();

        let project2 = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("b.qd", "content b")
            .expect("valid path")
            .add_source("a.qd", "content a")
            .expect("valid path")
            .add_source("main.qd", "main")
            .expect("valid path")
            .build()
            .unwrap();

        // Each path should have the same SourceId regardless of insertion order
        let path_a = VirtualPathBuf::parse("a.qd").unwrap();
        let path_b = VirtualPathBuf::parse("b.qd").unwrap();
        let path_main = VirtualPathBuf::parse("main.qd").unwrap();

        let id_a_1 = project1.sources().get_id(&path_a).unwrap();
        let id_a_2 = project2.sources().get_id(&path_a).unwrap();
        assert_eq!(id_a_1, id_a_2);

        let id_b_1 = project1.sources().get_id(&path_b).unwrap();
        let id_b_2 = project2.sources().get_id(&path_b).unwrap();
        assert_eq!(id_b_1, id_b_2);

        let id_main_1 = project1.sources().get_id(&path_main).unwrap();
        let id_main_2 = project2.sources().get_id(&path_main).unwrap();
        assert_eq!(id_main_1, id_main_2);

        // Entry SourceId should also be the same
        assert_eq!(
            project1.sources().get_id(project1.entry()).unwrap(),
            project2.sources().get_id(project2.entry()).unwrap()
        );
    }

    #[test]
    fn compile_result_is_independent_of_source_insertion_order() {
        let project1 = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("a.qd", "content a")
            .expect("valid path")
            .add_source("b.qd", "content b")
            .expect("valid path")
            .add_source("main.qd", "# Main\n\n{{ a.qd }} {{ b.qd }}")
            .expect("valid path")
            .build()
            .unwrap();

        let project2 = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("b.qd", "content b")
            .expect("valid path")
            .add_source("a.qd", "content a")
            .expect("valid path")
            .add_source("main.qd", "# Main\n\n{{ a.qd }} {{ b.qd }}")
            .expect("valid path")
            .build()
            .unwrap();

        let result1 = super::compile(&project1, &CompileOptions::default());
        let result2 = super::compile(&project2, &CompileOptions::default());

        // Serialize and compare
        let json1 = serde_json::to_string(&result1.ir).unwrap();
        let json2 = serde_json::to_string(&result2.ir).unwrap();
        assert_eq!(json1, json2);

        // Also verify all span SourceIds match
        for (span1, span2) in result1.ir.nodes.iter().zip(&result2.ir.nodes) {
            // Nodes should have same SourceIds in their spans
            assert_eq!(span1, span2);
        }
    }

    fn compile_source(source: &str) -> (crate::CompileResult, crate::SourceId) {
        let project = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", source)
            .expect("valid path")
            .build()
            .unwrap();
        let source_id = project.sources().get_id(project.entry()).unwrap();
        (
            super::compile(&project, &CompileOptions::default()),
            source_id,
        )
    }

    fn output_text(result: &crate::CompileResult) -> String {
        result
            .ir
            .nodes
            .iter()
            .filter_map(|node| match node {
                IrNode::Paragraph { content, .. } => Some(
                    content
                        .iter()
                        .filter_map(|inline| match inline {
                            IrInline::Text { content, .. } => Some(content.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn inline_text(content: &[IrInline]) -> String {
        content
            .iter()
            .map(|inline| match inline {
                IrInline::Text { content, .. } => content.clone(),
                IrInline::Strong { content, .. }
                | IrInline::Emphasis { content, .. }
                | IrInline::Strikethrough { content, .. } => inline_text(content),
                other => panic!("unexpected inline {other:?}"),
            })
            .collect()
    }

    #[test]
    fn compile_propagates_parser_diagnostics() {
        for (input, expected_code) in [
            (".foo {", "E2003"),
            (".foo width:{x} {y}", "E2001"),
            (".foo key:", "E2002"),
        ] {
            let (result, source_id) = compile_source(input);
            assert_eq!(result.diagnostics.len(), 1, "input {input:?}");
            let diag = &result.diagnostics[0];
            assert_eq!(diag.code, expected_code, "input {input:?}");
            assert!(matches!(diag.severity, Severity::Error), "input {input:?}");
            assert!(!diag.message.is_empty(), "input {input:?}");
            assert_eq!(
                diag.primary.as_ref().map(|s| s.source_id),
                Some(source_id),
                "input {input:?}"
            );
            // Malformed calls are not coerced into ordinary text or another
            // semantic node merely to produce IR.
            assert_eq!(result.ir.nodes.len(), 0, "input {input:?}");
        }
    }

    #[test]
    fn compile_reports_no_diagnostics_for_valid_input() {
        let (result, _) = compile_source(".foo {bar}\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
    }

    #[test]
    fn compile_evaluates_block_and_inline_chain_value_flow() {
        let source = ".sum {10} {5}::multiply {2}\n\nprefix .uppercase {hello}::lowercase suffix\n\n.uppercase {hello}::uppercase::lowercase\n";
        let (result, source_id) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);

        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!("expected block-chain paragraph")
        };
        assert!(matches!(
            content.as_slice(),
            [IrInline::Text { content, .. }] if content == "30"
        ));
        let first_span = match &result.ir.nodes[0] {
            IrNode::Paragraph { span, .. } => *span,
            _ => panic!("expected paragraph span"),
        };
        assert_eq!(
            first_span,
            scribium_source::SourceSpan::new(source_id, 0, source.find('\n').unwrap())
        );

        let IrNode::Paragraph { content, .. } = &result.ir.nodes[1] else {
            panic!("expected inline-chain paragraph")
        };
        assert!(content.iter().any(|inline| matches!(
            inline,
            IrInline::Text { content, .. } if content == "hello"
        )));

        let IrNode::Paragraph { content, .. } = &result.ir.nodes[2] else {
            panic!("expected three-chain paragraph")
        };
        assert!(matches!(
            content.as_slice(),
            [IrInline::Text { content, .. }] if content == "hello"
        ));
    }

    #[test]
    fn compile_evaluates_chain_inside_a_content_argument() {
        let source = ".var {value} {.uppercase {hello}::lowercase}\n.value\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!("expected content-chain result")
        };
        assert!(matches!(
            content.as_slice(),
            [IrInline::Text { content, .. }] if content == "hello"
        ));
    }

    #[test]
    fn compile_chain_and_nested_call_are_semantically_equivalent() {
        for (chain_source, nested_source, expected) in [
            (
                ".sum {10} {5}::multiply {2}\n",
                ".multiply {.sum {10} {5}} {2}\n",
                "30",
            ),
            (
                ".uppercase {hello}::lowercase\n",
                ".lowercase {.uppercase {hello}}\n",
                "hello",
            ),
        ] {
            let (chain, _) = compile_source(chain_source);
            let (nested, _) = compile_source(nested_source);
            assert!(chain.diagnostics.is_empty(), "{chain:?}");
            assert!(nested.diagnostics.is_empty(), "{nested:?}");
            assert_eq!(output_text(&chain), expected);
            assert_eq!(output_text(&nested), expected);
        }
    }

    #[test]
    fn compile_user_functions_support_zero_and_required_parameters() {
        let source = ".function {hello}\n    Hello\n\n.hello\n\n.function {greet}\n    to from:\n    .to from .from\n\n.greet {world} {John}\n.greet {world} from:{John}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(
            output_text(&result),
            "Hello\nworld from John\nworld from John"
        );
    }

    #[test]
    fn compile_let_supports_explicit_and_implicit_block_lambdas() {
        let source = ".let {Quarkdown}\n    name:\n    .uppercase {.name}\n\n.let {Quarkdown}\n    .uppercase {.1}\n\n.let {true}\n    condition:\n    .if {.condition}\n        yes\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "QUARKDOWN\nQUARKDOWN\nyes");
    }

    #[test]
    fn compile_let_preserves_content_results_and_parent_lookup() {
        let source = ".var {name} {outer}\n.function {decorate}\n    value:\n    .uppercase {.value}\n\n.let {inner}\n    name:\n    .name\n\n.name\n\n.let {hello}\n    value:\n    .decorate {.value}\n\n.let {Quarkdown}\n    name:\n    **Hello .name**\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "inner\nouter\nHELLO\n");
        let Some(IrNode::Paragraph { content, .. }) = result.ir.nodes.last() else {
            panic!("expected structured let result")
        };
        assert_eq!(inline_text(content), "Hello Quarkdown");
        assert!(matches!(content.as_slice(), [IrInline::Strong { .. }]));
    }

    #[test]
    fn compile_let_nested_scopes_use_nearest_implicit_argument() {
        let source = ".let {outer}\n    .let {.1}\n        .1\n\n.let {outer}\n    .let {.1}\n        value:\n        .value\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "outer\nouter");
    }

    #[test]
    fn compile_let_isolates_local_variables_and_functions() {
        let source = ".var {x} {outer}\n.let {inner}\n    value:\n    .var {x} {.value}\n    .x\n\n.x\n\n.let {hello}\n    value:\n    .function {local}\n        body:\n        .body\n\n.local\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "inner\nouter");
        let Some(IrNode::FunctionCall { name, .. }) = result.ir.nodes.last() else {
            panic!("expected local function reference to remain outside the let scope")
        };
        assert_eq!(name, "local");
    }

    #[test]
    fn compile_foreach_closed_range_is_inclusive_and_preserves_numbers() {
        for source in [
            ".foreach {2..4}\n    number:\n    .number\n",
            ".foreach {2..4}\n    .1\n",
        ] {
            let (result, _) = compile_source(source);
            assert!(result.diagnostics.is_empty(), "{result:?}");
            assert_eq!(output_text(&result), "2\n3\n4");
        }
    }

    #[test]
    fn compile_foreach_repeated_range_results_fail_once_at_output_boundary() {
        let source = ".foreach {1..3}\n    .let {2..4}\n        .1\n";
        let (result, source_id) = compile_source(source);
        let range_start = source.find("2..4").expect("range literal");
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                range_start,
                range_start + "2..4".len()
            ))
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_direct_range_output_reports_one_source_backed_failure() {
        let source = ".var {r} {2..4}\n.r\n";
        let (result, source_id) = compile_source(source);
        let range_start = source.find("2..4").expect("range literal");
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                range_start,
                range_start + "2..4".len()
            ))
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_range_composition_fails_without_fabricating_empty_content() {
        let source = ".let {ignored}\n    .var {r} {2..4}\n    .r\n    tail\n";
        let (result, source_id) = compile_source(source);
        let range_start = source.find("2..4").expect("range literal");
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                range_start,
                range_start + "2..4".len()
            ))
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_collection_composition_materializes_in_order_without_stringifying() {
        let source = ".let {ignored}\n    .var {c}\n        .foreach {1..2}\n            .1\n    .c\n    tail\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "1\n2\ntail");
    }

    #[test]
    fn compile_collection_access_operations_cover_basic_recursive_values() {
        let source = ".var {values}\n    - 1\n    - yes\n    - three\n\n.values::size\n.values::first\n.values::last\n.values::getat {2}\n.size of:{.values}\n.first from:{.values}\n.last from:{.values}\n.values::getat {9} orelse:{fallback}\nInline .values::first .values::last .values::size\n\n.var {matrix}\n    - - A\n      - B\n    - - C\n      - D\n\n.matrix::first::size\n.matrix::last::last\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(
            output_text(&result),
            "3\n1\nthree\nyes\n3\n1\nthree\nfallback\nInline 1 three 3\n2\nD"
        );
    }

    #[test]
    fn compile_collection_access_keeps_pair_dictionary_and_range_values_typed() {
        let source = ".var {pair}\n    .pair {left} {right}\n.var {table}\n    .dictionary\n        - a: 1\n        - b: 2\n.var {range} {2..4}\n\n.pair::size\n.pair::first\n.pair::last\n.table::size\n.table::getat {1}::first\n.table::getat {2}::last\n.range::size\n.range::first\n.range::last\n.range::getat {2}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "2\nleft\nright\n2\na\n2\n3\n2\n4\n3");
    }

    #[test]
    fn compile_collection_access_results_interoperate_with_foreach_and_functions() {
        let source = ".var {table}\n    .dictionary\n        - a: 1\n        - b: 2\n.var {mapped}\n    .foreach {1..3}\n        .1\n.var {matrix}\n    - - A\n      - B\n.function {measure}\n    value:\n    .value::size\n\n.mapped::size\n.foreach {.table::getat {1}}\n    .1\n.measure {.table}\n\n.let {.matrix}\n    .var {local} {.1::first::first}\n    .local\n.local\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "3\na\n1\n2\nA");
        assert!(result
            .ir
            .nodes
            .iter()
            .any(|node| matches!(node, IrNode::FunctionCall { name, .. } if name == "local")));
    }

    #[test]
    fn compile_collection_access_empty_and_invalid_inputs_are_atomic() {
        for source in [
            ".var {empty} {4..2}\n\n.empty::size\n.empty::first\n.empty::last\n.empty::getat {1}\n",
            ".size of:{true}\n",
            ".size of:{.unknown}\n",
            ".size of:{.multiply {true} {2}}\n",
            ".pair {a} {b}::getat {1.5}\n",
        ] {
            let (result, _) = compile_source(source);
            if source.starts_with(".var {empty}") {
                assert!(result.diagnostics.is_empty(), "{source:?}: {result:?}");
                assert_eq!(output_text(&result), "0\nNone\nNone\nNone");
            } else {
                assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
                assert_eq!(result.diagnostics[0].code, "E3001", "{source:?}");
                assert!(result.ir.nodes.is_empty(), "{source:?}: {result:?}");
            }
        }
    }

    #[test]
    fn compile_collection_access_diagnostics_keep_utf8_and_crlf_source_spans() {
        let source = ".size of:{세계}\r\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                0,
                source.find("\r\n").expect("line ending")
            ))
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_unresolved_range_argument_fails_before_typst_lowering() {
        let source = ".foo {2..4}\n";
        let (result, source_id) = compile_source(source);
        let range_start = source.find("2..4").expect("range literal");
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                range_start,
                range_start + "2..4".len()
            ))
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_foreach_returns_a_typed_collection_that_can_be_stored_and_consumed() {
        let source = ".var {mapped}\n    .foreach {1..3}\n        n:\n        .multiply {.n} by:{2}\n\n.mapped\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "2\n4\n6");
        assert!(matches!(
            result.ir.nodes.as_slice(),
            [
                IrNode::Paragraph { .. },
                IrNode::Paragraph { .. },
                IrNode::Paragraph { .. }
            ]
        ));
    }

    #[test]
    fn compile_foreach_reads_parent_values_and_functions_with_isolated_children() {
        let source = ".var {prefix} {item}\n.function {square}\n    n:\n    .multiply {.n} by:{.n}\n\n.foreach {1..3}\n    n:\n    .prefix .square {.n}\n\n.foreach {1..2}\n    n:\n    .var {local} {.n}\n    .local\n\n.local\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "item 1\nitem 4\nitem 9\n1\n2");
        assert!(result
            .ir
            .nodes
            .iter()
            .any(|node| { matches!(node, IrNode::FunctionCall { name, .. } if name == "local") }));
    }

    #[test]
    fn compile_foreach_adapts_only_list_values_and_preserves_nested_collections() {
        let source = ".var {letters}\n    1. A\n    2. B\n    3. C\n\n.foreach {.letters}\n    .1::lowercase\n\n.var {matrix}\n    - - A\n      - B\n    - - C\n      - D\n\n.foreach {.matrix}\n    .1\n\n- ordinary\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "a\nb\nc\nA\nB\nC\nD");
        assert!(result
            .ir
            .nodes
            .iter()
            .any(|node| { matches!(node, IrNode::UnorderedList { .. }) }));
    }

    #[test]
    fn compile_foreach_scopes_implicit_parameters_at_the_nearest_boundary() {
        let implicit = ".let {outer}\n    .foreach {1..2}\n        .1\n";
        let (result, _) = compile_source(implicit);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "1\n2");

        let explicit = ".let {outer}\n    .foreach {1..2}\n        n:\n        .1\n";
        let (result, _) = compile_source(explicit);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3003");
    }

    #[test]
    fn compile_dictionary_foreach_destructures_ordered_pairs() {
        let source = ".var {table}\n    .dictionary\n        - a: 1\n        - b: 2\n\n.foreach {.table}\n    key value:\n    .key: .value\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "a: 1\nb: 2");
        assert!(result
            .ir
            .nodes
            .iter()
            .all(|node| matches!(node, IrNode::Paragraph { .. })));
    }

    #[test]
    fn compile_dictionary_duplicate_keys_are_last_write_wins_in_first_slot() {
        let source = ".dictionary\n    - a: 1\n    - b: 2\n    - a: 3\n";
        let (result, source_id) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        let [IrNode::Table { rows, span, .. }] = result.ir.nodes.as_slice() else {
            panic!(
                "expected dictionary table output, got {:?}",
                result.ir.nodes
            )
        };
        assert_eq!(span.source_id, source_id);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.span.source_id == source_id));
        assert_eq!(inline_text(&rows[0].cells[0].content), "a");
        assert_eq!(inline_text(&rows[0].cells[1].content), "3");
        assert_eq!(inline_text(&rows[1].cells[0].content), "b");
        assert_eq!(inline_text(&rows[1].cells[1].content), "2");
    }

    #[test]
    fn compile_empty_dictionary_is_ordered_and_deterministic() {
        let source = ".dictionary\n";
        let (first, source_id) = compile_source(source);
        let (second, _) = compile_source(source);
        assert!(first.diagnostics.is_empty(), "{first:?}");
        assert!(second.diagnostics.is_empty(), "{second:?}");
        assert_eq!(first.ir, second.ir);
        let [IrNode::Table { header, rows, span }] = first.ir.nodes.as_slice() else {
            panic!("expected empty dictionary table, got {:?}", first.ir.nodes)
        };
        assert!(rows.is_empty());
        assert_eq!(span.source_id, source_id);
        assert_eq!(inline_text(&header.cells[0].content), "Key");
        assert_eq!(inline_text(&header.cells[1].content), "Value");
    }

    #[test]
    fn compile_recursive_dictionary_value_remains_typed_through_foreach() {
        let source = ".var {table}\n    .dictionary\n        - outer\n            - nested: 1\n\n.foreach {.table}\n    key value:\n    .value\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert!(matches!(
            result.ir.nodes.as_slice(),
            [IrNode::Table { rows, .. }] if rows.len() == 1
        ));
    }

    #[test]
    fn compile_pair_is_a_typed_recursive_value_at_the_output_boundary() {
        let source = ".pair {left} {.sum {1} {2}}\n";
        let (result, source_id) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        let [IrNode::OrderedList { items, .. }] = result.ir.nodes.as_slice() else {
            panic!(
                "expected Pair output as an ordered list, got {:?}",
                result.ir.nodes
            )
        };
        assert_eq!(items.len(), 2);
        assert!(matches!(
            items[0].nodes.as_slice(),
            [IrNode::Paragraph { .. }]
        ));
        assert!(matches!(
            items[1].nodes.as_slice(),
            [IrNode::Paragraph { .. }]
        ));
        assert!(matches!(
            items[0].nodes.as_slice(),
            [IrNode::Paragraph { content, span }]
                if inline_text(content) == "left" && span.source_id == source_id
        ));
        assert!(matches!(
            items[1].nodes.as_slice(),
            [IrNode::Paragraph { content, span }]
                if inline_text(content) == "3" && span.source_id == source_id
        ));
    }

    #[test]
    fn compile_dictionary_entry_failure_is_atomic_and_stops_before_output() {
        let source = ".dictionary\n    - a: 1\n    - b: .multiply {true} {2}\n    - c: 3\n";
        let (result, _) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_dictionary_implicit_scope_keeps_the_pair_typed() {
        let source = ".var {table}\n    .dictionary\n        - a: 1\n.foreach {.table}\n    .1\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert!(matches!(
            result.ir.nodes.as_slice(),
            [IrNode::OrderedList { items, .. }] if items.len() == 2
        ));
    }

    #[test]
    fn compile_dictionary_explicit_scope_masks_implicit_positional_references() {
        let source = ".var {table}\n    .dictionary\n        - a: 1\n.foreach {.table}\n    key value:\n    .1\n";
        let (result, _) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3003");
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_dictionary_destructuring_masks_and_restores_parent_bindings() {
        let source = ".var {key} {outer}\n.var {table}\n    .dictionary\n        - key: inner\n\n.foreach {.table}\n    key value:\n    .key\n\n.key\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "key\nouter");
    }

    #[test]
    fn compile_nested_dictionary_destructuring_restores_outer_scope() {
        let source = ".var {outer_table}\n    .dictionary\n        - outer\n            - nested: 1\n.var {inner_table}\n    .dictionary\n        - inner: 2\n\n.foreach {.outer_table}\n    key value:\n    .foreach {.inner_table}\n        key value:\n        .key\n    .key\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "inner\nouter");
    }

    #[test]
    fn compile_repeat_is_one_based_and_uses_the_shared_collection_result() {
        for source in [".repeat {3}\n    n:\n    .n\n", ".repeat {3}\n    .1\n"] {
            let (result, _) = compile_source(source);
            assert!(result.diagnostics.is_empty(), "{result:?}");
            assert_eq!(output_text(&result), "1\n2\n3");
        }

        let (result, _) = compile_source(".repeat {1}\n    .1\n");
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "1");
    }

    #[test]
    fn compile_repeat_zero_and_descending_ranges_are_empty_per_upstream_evidence() {
        for source in [".repeat {0}\n    .1\n", ".foreach {4..2}\n    .1\n"] {
            let (result, _) = compile_source(source);
            assert!(result.diagnostics.is_empty(), "{result:?}");
            assert!(result.ir.nodes.is_empty());
        }
    }

    #[test]
    fn compile_collection_transforms_through_frontend_and_first_class_lambda_values() {
        let source = ".map {1..3} by:{value: .value}\n\n.sorted {.map {1..3} by:{@lambda .1}}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "1\n2\n3\n1\n2\n3");
    }

    #[test]
    fn compile_collection_api_parity_uses_frontend_lists_and_shared_iterables() {
        let source = ".var {letters}\n    - A\n    - B\n    - A\n.var {numbers}\n    - 1\n    - 2\n    - 3\n.var {pair}\n    .pair {left} {right}\n.var {table}\n    .dictionary\n        - a: 1\n        - b: 2\n.var {negative} {.range {-2} {0}}\n\n.letters::second\n.letters::third\n.letters::distinct::size\n.letters::reversed::first\n.letters::groupvalues::size\n.numbers::sumall\n.numbers::average\n.pair::reversed::first\n.table::second::first\n.table::reversed::first::first\n.table::distinct::size\n.table::groupvalues::size\n.negative::reversed::first\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(
            output_text(&result),
            "B\nA\n2\nA\n2\n6\n2\nright\nb\nb\n2\n2\n0"
        );
    }

    #[test]
    fn compile_iteration_accepts_left_open_and_rejects_endless_ranges() {
        let (result, _) = compile_source(".foreach {..4}\n    .1\n");
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "1\n2\n3\n4");

        for source in [
            ".foreach {2..}\n    .1\n",
            ".foreach {..}\n    .1\n",
            ".foreach {.range from:{3}}\n    .1\n",
            ".foreach {.range}\n    .1\n",
            ".repeat {1.5}\n    .1\n",
            ".repeat {-1}\n    .1\n",
            ".foreach {1..2}\n    first second:\n    .first\n",
        ] {
            let (result, _) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
            assert_eq!(result.diagnostics[0].code, "E3001", "{source:?}");
        }
    }

    #[test]
    fn compile_dynamic_range_converges_with_literal_and_supports_signed_bounds() {
        let source = ".var {literal} {1..3}\n.var {dynamic} {.range {1} {3}}\n.var {left} {.range to:{3}}\n.var {signed} {.range {-3.9} {2.9}}\n\n.literal::size\n.dynamic::size\n.left::size\n.left::first\n.left::last\n.left::getat {2}\n.signed::size\n.signed::first\n.signed::last\n.signed::getat {4}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "3\n3\n3\n1\n3\n2\n6\n-3\n2\n0");
    }

    #[test]
    fn compile_literal_range_boundary_does_not_wrap() {
        let (result, _) = compile_source(".foreach {2147483647..2147483647}\n    .1\n");
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "2147483647");

        let (result, _) = compile_source(".foreach {2147483648..2147483648}\n    .1\n");
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert!(
            result.diagnostics[0].message.contains("endless Range"),
            "{result:?}"
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_dynamic_range_supports_nested_bounds_and_typed_interoperability() {
        let source = ".var {r} {.range {.sum {1} {1}} {.sum {2} {2}}}\n.function {makerange}\n    .range {2} {4}\n.var {pair}\n    .pair {.range {2} {4}} {value}\n.var {table}\n    .dictionary\n        - key: .range {2} {4}\n.var {scoped}\n    .let {.range {2} {4}}\n        .1\n\n.r::size\n.makerange::first\n.pair::first::size\n.table::getat {1}::last::size\n.scoped::getat {2}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "3\n2\n3\n3\n3");
    }

    #[test]
    fn compile_dynamic_range_rejects_invalid_shapes_and_preserves_atomic_failures() {
        for source in [
            ".range {1} {2} {3}\n",
            ".range unknown:{1}\n",
            ".range from:{1} from:{2}\n",
            ".range {1} from:{2}\n",
            ".range {true}\n",
            ".range {text}\n",
            ".range from:{.multiply {true} {2}} to:{3}\n",
            ".range from:{.unknown}\n",
        ] {
            let (result, _) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
            assert_eq!(result.diagnostics[0].code, "E3001", "{source:?}");
            assert!(result.ir.nodes.is_empty(), "{source:?}: {result:?}");
        }

        let source = ".foreach {.range from:{3}}\n    .multiply {true} {2}\n";
        let (result, _) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert!(
            result.diagnostics[0].message.contains("endless Range"),
            "{result:?}"
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");

        for source in [
            ".range from:{3}::size\n",
            ".range from:{3}::first\n",
            ".range from:{3}::last\n",
            ".range from:{3}::getat {1}\n",
            ".range::size\n",
        ] {
            let (result, _) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
            assert!(
                result.diagnostics[0].message.contains("endless Range"),
                "{source:?}: {result:?}"
            );
            assert!(result.ir.nodes.is_empty(), "{source:?}: {result:?}");
        }

        let source = ".function {makerange}\n    .range from:{.multiply {true} {2}} to:{3}\n.makerange::size\n";
        let (result, _) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001", "{result:?}");
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_dynamic_range_diagnostics_keep_utf8_crlf_and_nested_bound_spans() {
        let source = "앞 문장\r\n.range from:{.multiply {true} {2}} to:{3}\r\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let nested_start = source.find(".multiply").expect("nested bound");
        let nested_end = nested_start + ".multiply {true} {2}".len();
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                nested_start,
                nested_end
            ))
        );

        let source = "앞 문장\r\n.range from:{3}::size\r\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let range_start = source.find(".range").expect("range call");
        let range_end = range_start + ".range from:{3}".len();
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                range_start,
                range_end
            ))
        );
    }

    #[test]
    fn compile_iteration_body_no_value_and_failure_are_single_diagnostics() {
        for source in [
            ".foreach {1..3}\n    n:\n    .var {local} {.n}\n",
            ".foreach {1..3}\n    n:\n    .multiply {.n} by:{true}\n",
        ] {
            let (result, _) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
            assert_eq!(result.diagnostics[0].code, "E3001", "{source:?}");
            assert!(result.ir.nodes.is_empty(), "{source:?}: {result:?}");
        }
    }

    #[test]
    fn compile_iteration_fixture_qd_exercises_the_document_boundary() {
        let source = include_str!("../../../fixtures/markdown/quarkdown_iteration.qd");
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "2\n3\n4\n1\n2\n3\na\nb\nc");
    }

    #[test]
    fn compile_v251_string_scalar_fixture_preserves_typed_value_flow() {
        let source = include_str!(
            "../../../fixtures/quarkdown-conformance/cases/string-scalar-family/input.qd"
        );
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(
            output_text(&result),
            "  Hello  \nabcdef\nabc\nHello, world!\nHello world\ntrue\nstarts\ncase-sensitive\nignored\nempty\nspace"
        );
    }

    #[test]
    fn document_state_reads_empty_values_when_unset() {
        let (result, _) = compile_source(".docname\n.docdescription\n");
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "\n");
        assert_eq!(result.ir.metadata.document_state.name, "");
        assert_eq!(result.ir.metadata.document_state.description, "");
        assert_eq!(
            result.ir.metadata.document_state.document_type,
            crate::ir::IrDocumentType::Plain
        );
    }

    #[test]
    fn doctype_defaults_to_plain_and_writes_return_no_value() {
        let (default, _) = compile_source(".doctype\n");
        assert!(default.diagnostics.is_empty(), "{default:?}");
        assert_eq!(output_text(&default), "plain");
        assert_eq!(
            default.ir.metadata.document_state.document_type,
            crate::ir::IrDocumentType::Plain
        );

        let (written, _) = compile_source(".doctype {paged}\n.doctype\n");
        assert!(written.diagnostics.is_empty(), "{written:?}");
        assert_eq!(output_text(&written), "paged");
        assert_eq!(
            written.ir.metadata.document_state.document_type,
            crate::ir::IrDocumentType::Paged
        );

        let (named, _) = compile_source(".doctype type:{SLIDES}\n.doctype\n");
        assert!(named.diagnostics.is_empty(), "{named:?}");
        assert_eq!(output_text(&named), "slides");
        assert_eq!(
            named.ir.metadata.document_state.document_type,
            crate::ir::IrDocumentType::Slides
        );
    }

    #[test]
    fn invalid_doctype_is_atomic_and_static_string_does_not_gain_enum_meaning() {
        let source = ".doctype {paged}\n.doctype {book}\n.doctype\n";
        let (invalid, _) = compile_source(source);
        assert_eq!(invalid.diagnostics.len(), 1, "{invalid:?}");
        assert_eq!(output_text(&invalid), "paged");
        assert_eq!(
            invalid.ir.metadata.document_state.document_type,
            crate::ir::IrDocumentType::Paged
        );

        let (static_string, _) = compile_source(".doctype {.string {paged}}\n.doctype\n");
        assert_eq!(static_string.diagnostics.len(), 1, "{static_string:?}");
        assert_eq!(output_text(&static_string), "plain");
        assert_eq!(
            static_string.ir.metadata.document_state.document_type,
            crate::ir::IrDocumentType::Plain
        );
    }

    #[test]
    fn document_state_writes_return_no_value_and_reads_observe_commits() {
        let (writes, _) = compile_source(".docname {My document}\n.docdescription {A document}\n");
        assert!(writes.diagnostics.is_empty(), "{writes:?}");
        assert!(writes.ir.nodes.is_empty(), "{writes:?}");
        assert_eq!(writes.ir.metadata.document_state.name, "My document");
        assert_eq!(writes.ir.metadata.document_state.description, "A document");

        let (reads, _) = compile_source(
            ".docname {My document}\n.docdescription {A document}\n.docname\n.docdescription\n",
        );
        assert!(reads.diagnostics.is_empty(), "{reads:?}");
        assert_eq!(output_text(&reads), "My document\nA document");
    }

    #[test]
    fn document_state_is_shared_by_callable_child_scopes() {
        let source = ".function {rename}\n    .docname {inside}\n\n.rename\n.docname\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "inside");
        assert_eq!(result.ir.metadata.document_state.name, "inside");
    }

    #[test]
    fn blank_docname_fails_without_mutating_previous_state_or_losing_provenance() {
        let source = ".docname {valid}\n.docname {   }\n.docname\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let second_call = source.find(".docname {   }").expect("blank docname call");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                second_call,
                second_call + ".docname {   }".len(),
            ))
        );
        assert_eq!(output_text(&result), "valid");
        assert_eq!(result.ir.metadata.document_state.name, "valid");
    }

    #[test]
    fn document_state_snapshot_is_preserved_after_later_failure() {
        let source = ".docname {valid}\n.docdescription {description}\n.abs {invalid}\n";
        let (result, _) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.ir.metadata.document_state.name, "valid");
        assert_eq!(result.ir.metadata.document_state.description, "description");
    }

    #[test]
    fn compile_v251_plaintext_fixture_projects_evaluated_inline_content() {
        let source =
            include_str!("../../../fixtures/quarkdown-conformance/cases/plaintext-family/input.qd");
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(
            output_text(&result),
            "Hello, world!\none two three four five\nUse cargo test\nScribium\nA\nB\nAB\nHello WORLD\nnamed content\nblock body"
        );
    }

    #[test]
    fn compile_v251_optionality_fixture_preserves_lazy_typed_callbacks() {
        let source = include_str!(
            "../../../fixtures/quarkdown-conformance/cases/optionality-callback-family/input.qd"
        );
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "QUARKDOWN\nmissing\n4\nodd\n4");
    }

    #[test]
    fn compile_plaintext_rejects_unsupported_values_atomically() {
        for source in [
            ".plaintext {.pair {a} {b}}\n",
            ".plaintext {1..2}\n",
            ".plaintext {\"**hello**\"}\n",
        ] {
            let (result, source_id) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
            assert_eq!(result.diagnostics[0].code, "E3001", "{source:?}");
            assert_eq!(
                result.diagnostics[0].primary,
                Some(scribium_source::SourceSpan::new(
                    source_id,
                    0,
                    source.trim_end().len(),
                )),
                "{source:?}"
            );
            assert!(result.ir.nodes.is_empty(), "{source:?}: {result:?}");
        }
    }

    #[test]
    fn compile_v251_numeric_arithmetic_fixture_preserves_typed_value_flow() {
        let source = include_str!(
            "../../../fixtures/quarkdown-conformance/cases/numeric-arithmetic-family/input.qd"
        );
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(
            output_text(&result),
            "7\n3.5\n-1\n1\n3.5\n0\n1.4142135\n6\neven\nodd"
        );
    }

    #[test]
    fn compile_v251_dynamic_value_scalar_fixture_uses_existing_consumers() {
        let source = include_str!(
            "../../../fixtures/quarkdown-conformance/cases/dynamic-value-scalar-family/input.qd"
        );
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(
            output_text(&result),
            "3.5\nboolean conversion\n2\n3\n4\n2..4"
        );
    }

    #[test]
    fn compile_dynamic_and_static_string_origins_use_different_conversion_boundaries() {
        let positive = ".var {number-text} {.string {-3.5}}\n.abs {.number-text}\n\n.var {boolean-text} {.string {YES}}\n.if {.boolean-text}\n    boolean conversion\n\n.var {range-text} {.string {2..4}}\n.foreach {.range-text}\n    .1\n.size {.range-text}\n";
        let (result, _) = compile_source(positive);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "3.5\nboolean conversion\n2\n3\n4\n3");

        let (result, _) = compile_source(".var {chain-text} {.string {-3.5}}\n.chain-text::abs\n");
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "3.5");

        let (result, _) = compile_source(
            ".function {numeric-text}\n    .string {-3.5}\n\n.abs {.numeric-text}\n",
        );
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "3.5");

        for source in [
            ".abs {.string {-3.5}}\n",
            ".if {.string {YES}}\n    should not be emitted\n",
            ".range from:{.string {2}} to:{4}\n",
            ".foreach {.string {2..4}}\n    .1\n",
            ".string {-3.5}::abs\n",
        ] {
            let (result, _) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
            assert_eq!(result.diagnostics[0].code, "E3001", "{source:?}");
            assert!(result.diagnostics[0].primary.is_some(), "{source:?}");
            assert!(result.ir.nodes.is_empty(), "{source:?}: {result:?}");
        }
    }

    #[test]
    fn compile_dynamic_value_conversion_failures_are_atomic_and_source_backed() {
        for source in [
            "앞 문장\r\n.abs {.string {-3.5}}\r\n뒤 문장\r\n",
            "앞 문장\r\n.range from:{.string {2}} to:{4}\r\n뒤 문장\r\n",
            "앞 문장\r\n.if {.string {maybe}}\r\n    숨겨진 내용\r\n뒤 문장\r\n",
            "앞 문장\r\n.foreach {.string {2 .. 4}}\r\n    .1\r\n뒤 문장\r\n",
        ] {
            let (result, _) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{result:?}");
            assert_eq!(result.diagnostics[0].code, "E3001");
            assert!(result.diagnostics[0].primary.is_some(), "{result:?}");
            assert_eq!(output_text(&result), "앞 문장\n뒤 문장");
            assert!(!output_text(&result).contains("숨겨진 내용"));
            assert!(!output_text(&result).contains("1\n2\n3\n4"));
        }
    }

    #[test]
    fn compile_takeif_invokes_its_predicate_for_none() {
        let source = ".takeif {.none} {@lambda value: .sum {.value} {1}}\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                source.find(".sum").expect("predicate call"),
                source.find(".sum").expect("predicate call") + ".sum {.value} {1}".len()
            ))
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_v251_numeric_decimal_fixture_preserves_typed_value_flow() {
        let source = include_str!(
            "../../../fixtures/quarkdown-conformance/cases/numeric-decimal-family/input.qd"
        );
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(
            output_text(&result),
            "201.06\n201\n201\n-1.2\n2\n2\n4\n-2\n201.06\n2\n123"
        );
    }

    #[test]
    fn compile_v251_numeric_transcendental_fixture_preserves_typed_value_flow() {
        let source = include_str!(
            "../../../fixtures/quarkdown-conformance/cases/numeric-transcendental-family/input.qd"
        );
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(
            output_text(&result),
            "3.141592653589793\n3.14\n0\n0\n1\n0\n-1\n1\n1\n0.6931472"
        );
    }

    #[test]
    fn compile_numeric_decimal_forms_share_one_semantic_path() {
        let source = ".var {value} {201.06194}\n.truncate {.value} {2}\n.truncate {.value} decimals:{2}\n.round x:{3.5}\n.sum {2} {0.5}::round\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "201.06\n201.06\n4\n2");
    }

    #[test]
    fn compile_truncate_accepts_only_integral_dynamic_number_text() {
        let source = ".var {two-text} {.string {\"2\"}}\n.truncate {12.345} decimals:{.two-text}\n.var {two-point-zero-text} {.string {\"2.0\"}}\n.truncate {12.345} decimals:{.two-point-zero-text}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "12.34\n12.34");

        for source in [
            "앞 문장\r\n.var {fraction-text} {.string {\"1.5\"}}\r\n.truncate {12.345} decimals:{.fraction-text}\r\n뒤 문장\r\n",
            "앞 문장\r\n.truncate {12.345} decimals:{.string {\"2\"}}\r\n뒤 문장\r\n",
        ] {
            let (result, _) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
            assert_eq!(result.diagnostics[0].code, "E3001", "{source:?}");
            assert!(result.diagnostics[0].primary.is_some(), "{source:?}");
            assert_eq!(output_text(&result), "앞 문장\n뒤 문장", "{source:?}");
        }
    }

    #[test]
    fn compile_numeric_nested_failure_is_atomic_and_source_backed() {
        let source = "앞 문장\r\n.sum {.divide {10} by:{true}} {20}\r\n뒤 문장\r\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                source.find(".divide").expect("nested divide call"),
                source.find(".divide").expect("nested divide call")
                    + ".divide {10} by:{true}".len(),
            ))
        );
        assert_eq!(output_text(&result), "앞 문장\n뒤 문장");
        assert!(!output_text(&result).contains("20"));
    }

    #[test]
    fn compile_numeric_decimal_failure_is_atomic_and_source_backed() {
        let source = "앞 문장\r\n.sum {.truncate {12.34} decimals:{1.5}} {100}\r\n뒤 문장\r\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        let call_start = source.find(".truncate").expect("nested truncate call");
        let call_end = call_start + ".truncate {12.34} decimals:{1.5}".len();
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id, call_start, call_end
            ))
        );
        assert_eq!(output_text(&result), "앞 문장\n뒤 문장");
        assert!(!output_text(&result).contains("100"));
    }

    #[test]
    fn compile_numeric_transcendental_failure_is_atomic_and_source_backed() {
        let source = "앞 문장\r\n.sum {.sin {.multiply {10} by:{true}}} {20}\r\n뒤 문장\r\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        let call_start = source.find(".multiply").expect("nested multiply call");
        let call_end = call_start + ".multiply {10} by:{true}".len();
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id, call_start, call_end
            ))
        );
        assert_eq!(output_text(&result), "앞 문장\n뒤 문장");
        assert!(!output_text(&result).contains("20"));
    }

    #[test]
    fn compile_string_predicate_failure_is_atomic_and_source_backed() {
        let source = "앞 문장\r\n.if {.startswith {Hello} {he} ignorecase:{maybe}}\r\n    숨겨진 내용\r\n뒤 문장\r\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let diagnostic = &result.diagnostics[0];
        assert_eq!(diagnostic.code, "E3001");
        let call_start = source.find(".startswith").expect("startswith call");
        let call_end = call_start + ".startswith {Hello} {he} ignorecase:{maybe}".len();
        assert_eq!(
            diagnostic.primary,
            Some(scribium_source::SourceSpan::new(
                source_id, call_start, call_end
            ))
        );
        assert_eq!(output_text(&result), "앞 문장\n뒤 문장");
        assert!(!output_text(&result).contains("숨겨진 내용"));
    }

    #[test]
    fn compile_string_predicates_feed_lazy_conditionals_without_text_materialization() {
        let source = "\
.if {.isempty {\"\"}}\n\
    empty\n\
.ifnot {.isnotempty {\"\"}}\n\
    not-empty\n\
.if {.startswith {Hello} {he} ignorecase:{yes}}\n\
    case-insensitive\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "empty\nnot-empty\ncase-insensitive");
    }

    #[test]
    fn compile_let_reports_arity_and_implicit_parameter_spans() {
        let missing_value = ".let\n    value:\n    .value\n";
        let (result, source_id) = compile_source(missing_value);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3003");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                0,
                missing_value.trim_end().len()
            ))
        );

        let missing_implicit = ".let {1}\n    .2\n";
        let (result, source_id) = compile_source(missing_implicit);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let reference_start = missing_implicit.find(".2").expect("implicit reference");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                reference_start,
                reference_start + 2
            ))
        );

        let multiple_parameters = ".let {1}\n    first second:\n    .first\n";
        let (result, source_id) = compile_source(multiple_parameters);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let first_start = multiple_parameters.find("first").expect("first parameter");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                first_start,
                first_start + "first".len()
            ))
        );
    }

    #[test]
    fn compile_implicit_lambda_parameters_use_the_shared_callable_path() {
        let source = ".function {identity}\n    .1\n\n.identity {first}\n.identity {second}\n\n.function {pair}\n    .1\n    .2\n\n.pair {one} {two}\n\n.identity {2}::multiply {3}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "first\nsecond\none\ntwo\n6");
    }

    #[test]
    fn compile_implicit_parameters_preserve_typed_values() {
        let numeric = ".function {triple}\n    .multiply {.1} {3}\n\n.triple {2}\n";
        let (result, _) = compile_source(numeric);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "6");

        let boolean = ".function {truth}\n    .if {.1}\n        yes\n\n.truth {true}\n";
        let (result, _) = compile_source(boolean);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "yes");

        let none = ".function {optional}\n    value?:\n    .value\n\n.function {identity}\n    .1\n\n.function {is-none}\n    .isnone {.1}\n\n.is-none {.identity {.optional}}\n.is-none {\"None\"}\n";
        let (result, _) = compile_source(none);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "true\nfalse");
    }

    #[test]
    fn compile_implicit_parameter_content_keeps_markdown_structure() {
        let source = ".function {identity}\n    .1\n\n.identity\n    **rich**\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!("expected rich implicit parameter result")
        };
        assert!(matches!(content.as_slice(), [IrInline::Strong { .. }]));
        assert_eq!(inline_text(content), "rich");
    }

    #[test]
    fn compile_implicit_lambda_scopes_are_nested_and_reusable() {
        let source = ".function {inner}\n    .1\n\n.function {outer}\n    .inner {inner}\n    .1\n\n.outer {outer}\n.outer {again}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "inner\nouter\ninner\nagain");
    }

    #[test]
    fn compile_implicit_parameter_missing_and_zero_argument_are_diagnostics() {
        for source in [
            ".function {missing}\n    .2\n\n.missing {one}\n",
            ".function {zero}\n    .1\n\n.zero\n",
        ] {
            let (result, source_id) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
            let diagnostic = &result.diagnostics[0];
            assert_eq!(diagnostic.code, "E3003");
            assert_eq!(
                diagnostic.primary.map(|span| span.source_id),
                Some(source_id)
            );
            assert!(diagnostic.message.contains("Implicit lambda parameter"));
            assert!(result.ir.nodes.is_empty());
        }
    }

    #[test]
    fn compile_implicit_parameter_diagnostic_preserves_utf8_and_crlf_span() {
        let source = ".function {missing}\r\n    .2\r\n\r\n.missing {세계}\r\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let start = source.find(".2").expect("implicit parameter span");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                start,
                start + 2
            ))
        );
    }

    #[test]
    fn compile_implicit_parameters_keep_container_and_md_boundaries() {
        let source = ".function {identity}\n    .1\n\n- .identity {list}\n\n> .identity {quote}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(matches!(
            result.ir.nodes.first(),
            Some(IrNode::UnorderedList { items, .. }) if items.len() == 1
        ));
        assert!(matches!(
            result.ir.nodes.get(1),
            Some(IrNode::Blockquote { content, .. }) if !content.is_empty()
        ));

        let md_source = ".function {identity}\n    .1\n\n.identity {value}\n";
        let project = VirtualProjectBuilder::new()
            .entry("main.md")
            .expect("valid path")
            .add_source("main.md", md_source)
            .expect("valid source")
            .build()
            .expect("valid project");
        let result = super::compile(&project, &CompileOptions::default());
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result
            .ir
            .nodes
            .iter()
            .all(|node| !matches!(node, IrNode::FunctionDeclaration { .. })));
        assert!(output_text(&result).contains(".function"));
        assert!(output_text(&result).contains(".identity"));
    }

    #[test]
    fn compile_user_functions_keep_scalar_values_for_nested_and_chain_calls() {
        let source = ".function {area}\n    width height:\n    .multiply {.width} by:{.height}\n\n.sum {.area {4} {2}} {1}\n\n.area {4} {2}::sum {1}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "9\n9");
        assert!(result
            .ir
            .nodes
            .iter()
            .all(|node| { !matches!(node, IrNode::FunctionDeclaration { .. }) }));
    }

    #[test]
    fn compile_user_function_multi_statement_body_preserves_last_semantic_value() {
        let source = ".function {f}\n    .var {x} {2}\n    .sum {.x} {1}\n\n.sum {.f} {1}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "4");

        let source = ".function {f}\n    .function {local}\n        body\n    .sum {2} {1}\n\n.sum {.f} {1}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "4");
    }

    #[test]
    fn compile_user_function_multi_statement_body_stops_after_first_failure() {
        let source = ".function {bad}\n    .multiply {true} {true}\n    .var {after} {ran}\n\n.sum {.bad} {1}\n.after\n";
        let (result, _) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert!(result.diagnostics[0]
            .message
            .contains("requires numeric arguments"));
        assert!(!output_text(&result).contains("ran"));
    }

    #[test]
    fn compile_user_function_multi_statement_rich_content_keeps_source_spans() {
        let source = ".function {rich}\n    First **one**\n\n    Second *two*\n\n.rich\n";
        let (result, source_id) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        // Rushdown's original inline spans are retained verbatim; in
        // particular, the closing delimiter is not part of these paragraph
        // ranges, so assert against the exact source-backed range.
        let expected = ["First **one", "Second *two"];
        assert_eq!(result.ir.nodes.len(), expected.len());
        for (node, expected) in result.ir.nodes.iter().zip(expected) {
            let IrNode::Paragraph { span, .. } = node else {
                panic!("expected paragraph, got {node:?}")
            };
            assert_eq!(span.source_id, source_id);
            assert_eq!(&source[span.start..span.end], expected);
        }
    }

    #[test]
    fn compile_user_function_rich_and_block_results_keep_markdown_structure() {
        let rich_source = ".function {greet}\n    name:\n    **Hello, .name!**\n\n.greet {world}\n";
        let (rich, _) = compile_source(rich_source);
        assert!(rich.diagnostics.is_empty(), "{:?}", rich.diagnostics);
        let IrNode::Paragraph { content, .. } = &rich.ir.nodes[0] else {
            panic!("expected rich function result")
        };
        assert!(matches!(content.as_slice(), [IrInline::Strong { .. }]));
        assert_eq!(inline_text(content), "Hello, world!");

        let block_source = ".function {wrapper}\n    title content:\n    .content\n\n.wrapper {Title}\n    **Body**\n";
        let (block, _) = compile_source(block_source);
        assert!(block.diagnostics.is_empty(), "{:?}", block.diagnostics);
        let IrNode::Paragraph { content, .. } = &block.ir.nodes[0] else {
            panic!("expected block function result")
        };
        assert!(matches!(content.as_slice(), [IrInline::Strong { .. }]));
        assert_eq!(inline_text(content), "Body");

        let inline_source = ".function {inline_greet}\n    name:\n    **Hello, .name!**\n\nprefix .inline_greet {world} suffix\n";
        let (inline, _) = compile_source(inline_source);
        assert!(inline.diagnostics.is_empty(), "{:?}", inline.diagnostics);
        let IrNode::Paragraph { content, .. } = &inline.ir.nodes[0] else {
            panic!("expected inline function result")
        };
        assert!(content
            .iter()
            .any(|inline| { matches!(inline, IrInline::Strong { .. }) }));

        let unsupported_inline = ".function {heading}\n    # Heading\n\nprefix .heading suffix\n";
        let (unsupported, _) = compile_source(unsupported_inline);
        assert_eq!(unsupported.diagnostics.len(), 1);
        assert_eq!(unsupported.diagnostics[0].code, "E3003");
        assert!(unsupported.diagnostics[0]
            .message
            .contains("Rich block content"));

        let multiple_paragraphs =
            ".function {two}\n    First\n\n    Second\n\nprefix .two suffix\n";
        let (multiple, _) = compile_source(multiple_paragraphs);
        assert_eq!(multiple.diagnostics.len(), 1, "{multiple:?}");
        assert!(!output_text(&multiple).contains("First"));
        assert!(!output_text(&multiple).contains("Second"));
    }

    #[test]
    fn compile_user_functions_use_source_order_and_override_builtins() {
        let redeclaration = ".function {answer}\n    first\n\n.answer\n\n.function {answer}\n    second\n\n.answer\n";
        let (result, _) = compile_source(redeclaration);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "first\nsecond");

        let override_source = ".uppercase {Quarkdown}\n\n.function {uppercase}\n    text:\n    .text::lowercase\n\n.uppercase {Quarkdown}\n";
        let (result, _) = compile_source(override_source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "QUARKDOWN\nquarkdown");
    }

    #[test]
    fn compile_user_functions_bind_block_last_and_isolate_child_scope() {
        let source = ".var {outside} {A}\n.var {value} {parent}\n.function {inner}\n    inherited\n\n.function {demo}\n    value:\n    .function {local}\n        local\n    .outside\n    .value\n    .inner\n    .var {local_value} {.value}\n    .local\n\n.demo {B}\n\n.outside\n.value\n.local\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let output = output_text(&result);
        assert!(output.contains("A"), "{output:?}");
        assert!(output.contains("B"), "{output:?}");
        assert!(output.contains("inherited"), "{output:?}");
        assert!(
            output.ends_with("parent"),
            "shadowed parent changed: {output:?}"
        );
        assert!(result
            .ir
            .nodes
            .iter()
            .any(|node| { matches!(node, IrNode::FunctionCall { name, .. } if name == "local") }));
    }

    #[test]
    fn compile_captured_callable_uses_definition_fallback_and_caller_shadowing() {
        let source = r#".var {value} {definition}
.function {captured}
    .value

.captured
.var {value} {caller}
.captured
"#;
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "definition\ncaller");
    }

    #[test]
    fn compile_invocation_parameters_shadow_caller_and_definition_bindings() {
        let source = r#".var {value} {definition}
.function {show}
    value:
    .value

.var {value} {caller}
.show {parameter}
"#;
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "parameter");
    }

    #[test]
    fn compile_captured_callable_sees_caller_defined_function() {
        let source = r#".function {outer}
    .helper

.function {helper}
    caller helper

.outer
"#;
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "caller helper");
    }

    #[test]
    fn compile_nested_callable_propagates_caller_overlay_without_leaking_it() {
        let source = r#".function {inner}
    .value

.function {outer}
    .inner

.var {value} {caller}
.outer
.var {value} {next caller}
.outer
"#;
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "caller\nnext caller");
    }

    #[test]
    fn compile_nested_implicit_parameters_use_the_nearest_available_binding() {
        let source = r#".function {inner}
    .1

.function {outer}
    .inner

.let {caller}
    .outer {outer}

.function {nested}
    .1

.function {invoker}
    .nested {nested}

.let {caller}
    .invoker {outer}
"#;
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "outer\nnested");
    }

    #[test]
    fn compile_explicit_lambda_parameters_mask_outer_implicit_parameters() {
        let source = r#".let {outer}
    .let {inner}
        value:
        .1
"#;
        let (result, _) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3003");
        assert!(result.ir.nodes.is_empty());
    }

    #[test]
    fn compile_user_function_no_value_and_failed_nested_calls_keep_original_diagnostic() {
        let no_value = ".function {noop}\n    .var {temporary} {value}\n\n.sum {.noop} {1}\n";
        let (result, _) = compile_source(no_value);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert!(result.diagnostics[0].message.contains("no value"));

        let declaration_no_value =
            ".function {noop}\n    .function {local}\n        body\n\n.sum {.noop} {1}\n";
        let (result, _) = compile_source(declaration_no_value);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert!(result.diagnostics[0].message.contains("no value"));

        let failed = ".function {bad}\n    .multiply {true} {true}\n\n.sum {.bad} {1}\n";
        let (result, _) = compile_source(failed);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert!(result.diagnostics[0]
            .message
            .contains("requires numeric arguments"));
    }

    #[test]
    fn compile_user_function_argument_failures_are_single_and_body_is_not_run() {
        for (source, expected_message) in [
            (
                ".function {needs}\n    first:\n    .multiply {true} {true}\n\n.needs\n",
                "Missing required argument",
            ),
            (
                ".function {needs}\n    first:\n    .multiply {true} {true}\n\n.needs {one} {two}\n",
                "too many positional arguments",
            ),
            (
                ".function {needs}\n    first:\n    .multiply {true} {true}\n\n.needs unknown:{one}\n",
                "Unknown named parameter",
            ),
            (
                ".function {needs}\n    first:\n    .multiply {true} {true}\n\n.needs {one} first:{two}\n",
                "bound more than once",
            ),
        ] {
            let (result, _) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
            assert_eq!(result.diagnostics[0].code, "E3003");
            assert!(result.diagnostics[0].message.contains(expected_message));
            assert!(!result.diagnostics[0].message.contains("requires numeric arguments"));
        }
    }

    #[test]
    fn compile_user_function_declaration_errors_are_explicit_and_source_backed() {
        for source in [
            ".function {1invalid}\n    body\n",
            ".function {duplicate}\n    first first:\n    body\n",
            ".function {missing-body}\n",
            ".function {named} extra:{value}\n    body\n",
            ".function {named}::sum {1}\n    body\n",
        ] {
            let (result, source_id) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}: {result:?}");
            assert_eq!(result.diagnostics[0].code, "E3003");
            assert_eq!(
                result.diagnostics[0]
                    .primary
                    .as_ref()
                    .map(|span| span.source_id),
                Some(source_id)
            );
        }

        let source = ".function {named}\n    value:\n    body\n\n.named unknown:{value}\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let diagnostic = &result.diagnostics[0];
        assert_eq!(diagnostic.code, "E3003");
        let start = source.find("unknown").expect("named argument name");
        assert_eq!(
            diagnostic.primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                start,
                start + "unknown".len()
            ))
        );
    }

    #[test]
    fn compile_optional_user_parameters_bind_missing_positional_and_named_values() {
        let source = ".function {greet}\n    to from?:\n    Hello, .to from .from!\n\n.greet {world}\n.greet {world} {John}\n.greet {world} from:{Jane}\n\n.function {ordered}\n    first? second:\n    .first::otherwise {missing} .second\n\n.ordered second:{provided}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(
            output_text(&result),
            "Hello, world from None!\nHello, world from John!\nHello, world from Jane!\nmissing provided"
        );
    }

    #[test]
    fn compile_optional_parameters_support_otherwise_and_preserve_value_types() {
        let source = ".function {greet}\n    to from?:\n    Hello, .to from .from::otherwise {unnamed}!\n\n.greet {world}\n.greet {world} {John}\n\n.function {f}\n    x?:\n    .x::otherwise {42}\n\n.sum {.f} {1}\n\n.function {g}\n    value?:\n    .value\n\n.uppercase {.g::otherwise {fallback}}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(
            output_text(&result),
            "Hello, world from unnamed!\nHello, world from John!\n43\nFALLBACK"
        );
    }

    #[test]
    fn compile_optional_none_is_distinct_from_no_value() {
        let none_source = ".function {f}\n    x?:\n    .x\n\n.sum {.f} {1}\n";
        let (none_result, _) = compile_source(none_source);
        assert_eq!(none_result.diagnostics.len(), 1, "{none_result:?}");
        assert_eq!(none_result.diagnostics[0].code, "E3001");
        assert!(none_result.diagnostics[0]
            .message
            .contains("requires numeric arguments"));
        assert!(!none_result.diagnostics[0].message.contains("no value"));

        let no_value_source = ".function {f}\n    .var {local} {1}\n\n.sum {.f} {1}\n";
        let (no_value_result, _) = compile_source(no_value_source);
        assert_eq!(no_value_result.diagnostics.len(), 1, "{no_value_result:?}");
        assert_eq!(no_value_result.diagnostics[0].code, "E3001");
        assert!(no_value_result.diagnostics[0].message.contains("no value"));
    }

    #[test]
    fn compile_required_parameter_stays_required_after_optional_support() {
        let source = ".function {f}\n    required optional?:\n    .required\n\n.f\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let diagnostic = &result.diagnostics[0];
        assert_eq!(diagnostic.code, "E3003");
        assert!(diagnostic
            .message
            .contains("Missing required argument `required`"));
        let parameter_start = source.find("required").expect("required parameter");
        assert_eq!(
            diagnostic.primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                parameter_start,
                parameter_start + "required".len()
            ))
        );
    }

    #[test]
    fn compile_optional_final_parameter_accepts_missing_or_block_content_and_keeps_collision() {
        let source = ".function {wrap}\n    title content?:\n    .content::otherwise {empty}\n\n.wrap {Title}\n.wrap {Title}\n    Body\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "empty\nBody");

        let collision =
            ".function {wrap}\n    content?:\n    .content\n\n.wrap {explicit}\n    body\n";
        let (result, _) = compile_source(collision);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3003");
        assert!(result.diagnostics[0].message.contains("collides"));
    }

    #[test]
    fn compile_optional_none_can_be_stored_locally_without_parent_scope_leak() {
        let source = ".function {f}\n    value?:\n    .var {local} {.value}\n    .local::otherwise {fallback}\n\n.f\n.local\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "fallback");
        assert!(result
            .ir
            .nodes
            .iter()
            .any(|node| { matches!(node, IrNode::FunctionCall { name, .. } if name == "local") }));
    }

    #[test]
    fn compile_optional_none_direct_output_materializes_as_text() {
        let source = ".function {f}\n    value?:\n    .value\n\n.f\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "None");
    }

    #[test]
    fn compile_isnone_returns_a_semantic_boolean_for_optional_values() {
        let source = ".function {f}\n    value?:\n    .value::isnone\n\n.f\n.f {hello}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "true\nfalse");
    }

    #[test]
    fn compile_optionality_callbacks_reuse_typed_values_and_lazy_none() {
        let source = ".ifpresent {hello} {@lambda value: .uppercase {.value}}\n\
.ifpresent {.none} {@lambda x: .x::uppercase}::otherwise {fallback}\n\
.takeif {4} condition:{@lambda x: .x::iseven}\n\
.takeif {5} {@lambda x: .x::iseven}::otherwise {fallback}\n\
.takeif {4}\n    .iseven {.1}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "HELLO\nfallback\n4\nfallback\n4");
    }

    #[test]
    fn compile_optionality_callbacks_capture_and_shadow_lexical_scope() {
        let source = ".var {suffix} {!}\n\
.ifpresent {hello} {@lambda value: .concatenate {.value} {.suffix}}\n\
.var {value} {outer}\n\
.ifpresent {inner} {@lambda value: .value}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "hello!\ninner");
    }

    #[test]
    fn compile_takeif_none_still_invokes_condition() {
        let source = ".takeif {.none} {@lambda value: .sum {true} {2}}\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        let failure_start = source.find(".sum").expect("callback failure span");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                failure_start,
                failure_start + ".sum {true} {2}".len()
            ))
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_optionality_callback_failure_is_atomic_and_source_backed() {
        let source = ".ifpresent {hello} {@lambda .sum {true} {2}}\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        let failure_start = source.find(".sum").expect("callback failure span");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                failure_start,
                failure_start + ".sum {true} {2}".len()
            ))
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn compile_optionality_callback_failure_preserves_utf8_crlf_provenance() {
        let source = ".takeif {세계}\r\n    .sum {true} {2}\r\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3001");
        let failure_start = source.find(".sum").expect("callback failure span");
        assert_eq!(
            result.diagnostics[0].primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                failure_start,
                failure_start + ".sum {true} {2}".len()
            ))
        );
        assert!(result.ir.nodes.is_empty(), "{result:?}");
    }

    #[test]
    fn optional_parameter_spans_survive_utf8_and_crlf_frontend_to_ir_conversion() {
        let source = ".function {greet}\r\n    from? name:\r\n    안녕, .from .name!\r\n\r\n.greet {세계} {친구}\r\n";
        let project = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", source)
            .expect("valid path")
            .build()
            .expect("valid project");
        let source_id = project
            .sources()
            .get_id(project.entry())
            .expect("source id");
        let parsed = scribium_markdown::parse_with_diagnostics(source);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let (ir, diagnostics) = crate::ast_to_ir::ast_to_ir_with_diagnostics_for_mode(
            &parsed.document,
            source_id,
            project.metadata(),
            SourceMode::Quarkdown,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let IrNode::FunctionDeclaration { parameters, .. } = &ir.nodes[0] else {
            panic!("expected function declaration")
        };
        assert!(parameters[0].optional);
        assert_eq!(
            &source[parameters[0].span.start..parameters[0].span.end],
            "from?"
        );
        assert_eq!(
            &source[parameters[1].span.start..parameters[1].span.end],
            "name"
        );

        let result = super::compile(&project, &CompileOptions::default());
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "안녕, 세계 친구!");
    }

    #[test]
    fn compile_markdown_mode_does_not_enable_quarkdown_functions() {
        let project = VirtualProjectBuilder::new()
            .entry("main.md")
            .expect("valid path")
            .add_source(
                "main.md",
                ".function {hello}\n    value?:\n    Hello .value\n\n.hello\n",
            )
            .expect("valid path")
            .build()
            .unwrap();
        let result = super::compile(&project, &CompileOptions::default());
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result
            .ir
            .nodes
            .iter()
            .all(|node| { !matches!(node, IrNode::FunctionDeclaration { .. }) }));
    }

    #[test]
    fn compile_variable_values_keep_types_across_chain_and_nested_forms() {
        for (chain_source, nested_source, expected) in [
            (
                ".var {myvar} {hello!}\n.myvar::uppercase\n",
                ".var {myvar} {hello!}\n.uppercase {.myvar}\n",
                "HELLO!",
            ),
            (
                ".var {myvar} {true}\n.myvar::uppercase\n",
                ".var {myvar} {true}\n.uppercase {.myvar}\n",
                "TRUE",
            ),
        ] {
            let (chain, _) = compile_source(chain_source);
            let (nested, _) = compile_source(nested_source);
            assert!(chain.diagnostics.is_empty(), "{chain:?}");
            assert!(nested.diagnostics.is_empty(), "{nested:?}");
            assert_eq!(output_text(&chain), expected);
            assert_eq!(output_text(&nested), expected);
        }
    }

    #[test]
    fn compile_numeric_variable_reassignment_preserves_numeric_value_context() {
        let source = ".var {mynumber} {5}\n.mynumber {.mynumber::sum {1}}\n.mynumber::sum {1}\n";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(output_text(&result), "7");
    }

    #[test]
    fn compile_final_chain_no_value_is_legal_but_non_final_is_not() {
        let (final_result, _) = compile_source(".var {x} {0}\n.sum {1} {2}::x\n.x\n");
        assert!(final_result.diagnostics.is_empty(), "{final_result:?}");
        assert_eq!(output_text(&final_result), "3");

        let (non_final_result, _) = compile_source(".var {x} {0}\n.sum {1} {2}::x::sum {1}\n.x\n");
        assert_eq!(non_final_result.diagnostics.len(), 1);
        assert_eq!(non_final_result.diagnostics[0].code, "E3001");
        assert_eq!(output_text(&non_final_result), "3");
    }

    #[test]
    fn compile_nested_no_value_matches_chain_failure_classification() {
        let (nested_result, _) = compile_source(".var {x} {0}\n.multiply {.x {3}} {2}\n.x\n");
        assert_eq!(nested_result.diagnostics.len(), 1, "{nested_result:?}");
        assert_eq!(nested_result.diagnostics[0].code, "E3001");
        assert_eq!(output_text(&nested_result), "3");

        let (failed_child, _) = compile_source(".multiply {.sum {true}} {2}\n");
        assert_eq!(failed_child.diagnostics.len(), 1, "{failed_child:?}");
        assert_eq!(failed_child.diagnostics[0].code, "E3001");
        assert!(failed_child.diagnostics[0]
            .message
            .contains("requires numeric arguments"));
    }

    #[test]
    fn compile_chain_and_ordinary_conditional_are_equally_lazy() {
        let chain_source =
            ".var {flag} {false}\n.var {x} {before}\n.flag::if\n    .x {after}\n.x\n";
        let ordinary_source =
            ".var {flag} {false}\n.var {x} {before}\n.if {.flag}\n    .x {after}\n.x\n";
        let (chain, _) = compile_source(chain_source);
        let (ordinary, _) = compile_source(ordinary_source);
        assert!(chain.diagnostics.is_empty(), "{chain:?}");
        assert!(ordinary.diagnostics.is_empty(), "{ordinary:?}");
        assert_eq!(output_text(&chain), "before");
        assert_eq!(output_text(&ordinary), "before");
    }

    #[test]
    fn chain_gate_removal_does_not_remove_other_e8001_diagnostics() {
        let (result, _) = compile_source("![image](image.png)\n");
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "E8001" && diagnostic.message.contains("image")
        }));
    }

    #[test]
    fn compile_reports_unimplemented_chain_callees_with_specific_spans() {
        for source in [".a::b\n", ".a::b::c\n", ".a {x}::b {y}\n"] {
            let parsed = scribium_markdown::parse_qd(source);
            let scribium_markdown::ast::Block::DirectiveCall { chain, .. } = &parsed.nodes[0]
            else {
                panic!("expected parsed block chain for {source:?}");
            };
            assert!(!chain.is_empty(), "{source:?}");

            let (result, source_id) = compile_source(source);
            assert_eq!(result.diagnostics.len(), 1, "{source:?}");
            let diagnostic = &result.diagnostics[0];
            assert_eq!(diagnostic.code, "E3001");
            assert!(matches!(diagnostic.severity, Severity::Error));
            assert!(diagnostic.message.contains("no semantic implementation"));
            assert_eq!(
                diagnostic.primary,
                Some(scribium_source::SourceSpan::new(source_id, 0, 2))
            );
            assert!(result.ir.nodes.is_empty());
        }
    }

    #[test]
    fn compile_reports_chain_failures_in_inline_and_content_paths() {
        let inline_source = "prefix .a {x}::b {y} suffix\n";
        let parsed = scribium_markdown::parse_qd(inline_source);
        let scribium_markdown::ast::Block::Paragraph { content, .. } = &parsed.nodes[0] else {
            panic!("expected inline paragraph");
        };
        assert!(content.iter().any(|inline| matches!(
            inline,
            scribium_markdown::ast::Inline::DirectiveCall { chain, .. }
                if !chain.is_empty()
        )));
        let (result, source_id) = compile_source(inline_source);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert!(matches!(result.diagnostics[0].severity, Severity::Error));
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!("expected inline paragraph IR");
        };
        assert!(content
            .iter()
            .all(|inline| !matches!(inline, IrInline::ChainedDirectiveCall { .. })));
        assert_eq!(
            result.diagnostics[0].primary.as_ref().unwrap().source_id,
            source_id
        );

        let content_source = ".outer {.a::b}\n";
        let parsed = scribium_markdown::parse_qd(content_source);
        let scribium_markdown::ast::Block::DirectiveCall {
            positional_args, ..
        } = &parsed.nodes[0]
        else {
            panic!("expected outer call");
        };
        let scribium_markdown::ast::Value::Content(content) = &positional_args[0] else {
            panic!("expected content argument");
        };
        assert!(content.iter().any(|inline| matches!(
            inline,
            scribium_markdown::ast::Inline::DirectiveCall { chain, .. }
                if !chain.is_empty()
        )));

        let (result, source_id) = compile_source(content_source);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, "E3001");
        assert_eq!(
            result.diagnostics[0].primary.as_ref().unwrap().source_id,
            source_id
        );
        assert!(result.ir.nodes.is_empty());
    }

    #[test]
    fn compile_qd_uses_the_production_frontend_pipeline() {
        let project = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", "# Hello\n.note {hello}\n")
            .expect("valid path")
            .build()
            .unwrap();

        let result = super::compile(&project, &CompileOptions::default());
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(result.ir.nodes.len(), 2);
        assert!(
            matches!(result.ir.nodes[1], IrNode::FunctionCall { ref name, .. } if name == "note")
        );
    }

    #[test]
    fn compile_md_uses_markdown_mode_through_the_production_frontend() {
        let project = VirtualProjectBuilder::new()
            .entry("main.md")
            .expect("valid path")
            .add_source("main.md", "# Hello\n\n**world**\n")
            .expect("valid path")
            .build()
            .unwrap();

        let result = super::compile(&project, &CompileOptions::default());
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert_eq!(result.ir.nodes.len(), 2);
        assert!(matches!(result.ir.nodes[0], IrNode::Heading { .. }));
        assert!(matches!(result.ir.nodes[1], IrNode::Paragraph { .. }));
    }

    #[test]
    fn compile_raw_html_mode_follows_case_insensitive_entry_extensions() {
        let source = "before <em>x</em> after\n";
        for entry in ["main.md", "main.MD", "main.Md"] {
            let project = VirtualProjectBuilder::new()
                .entry(entry)
                .expect("valid path")
                .add_source(entry, source)
                .expect("valid path")
                .build()
                .expect("valid project");
            let result = super::compile(&project, &CompileOptions::default());
            assert!(
                result
                    .diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.code != "E8001"),
                "{entry}: {result:?}"
            );
            let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
                panic!("{entry}: expected paragraph, got {:?}", result.ir.nodes);
            };
            assert!(
                content
                    .iter()
                    .any(|inline| matches!(inline, IrInline::Emphasis { .. })),
                "{entry}: expected Markdown emphasis, got {content:?}"
            );
        }

        for entry in ["main.qd", "main.QD", "main.scrib", "main.SCRIB"] {
            let project = VirtualProjectBuilder::new()
                .entry(entry)
                .expect("valid path")
                .add_source(entry, source)
                .expect("valid path")
                .build()
                .expect("valid project");
            let result = super::compile(&project, &CompileOptions::default());
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code == "E8001"),
                "{entry}: expected unsupported raw HTML, got {:?}",
                result.diagnostics
            );
            let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
                panic!("{entry}: expected paragraph, got {:?}", result.ir.nodes);
            };
            assert!(content
                .iter()
                .all(|inline| !matches!(inline, IrInline::Emphasis { .. })));
        }
    }

    #[test]
    fn compile_html_comment_noop_is_markdown_only_for_case_insensitive_entries() {
        let inline_source = "before <!-- note --> after\n";
        for entry in ["main.md", "main.MD", "main.Md"] {
            let project = VirtualProjectBuilder::new()
                .entry(entry)
                .expect("valid path")
                .add_source(entry, inline_source)
                .expect("valid path")
                .build()
                .expect("valid project");
            let result = super::compile(&project, &CompileOptions::default());
            assert!(result.diagnostics.is_empty(), "{entry}: {result:?}");
            let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
                panic!("{entry}: expected paragraph, got {:?}", result.ir.nodes);
            };
            assert_eq!(
                content
                    .iter()
                    .filter_map(|inline| match inline {
                        IrInline::Text { content, .. } => Some(content.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                vec!["before ", " after"]
            );
        }

        for entry in ["main.qd", "main.QD", "main.scrib", "main.SCRIB"] {
            let project = VirtualProjectBuilder::new()
                .entry(entry)
                .expect("valid path")
                .add_source(entry, inline_source)
                .expect("valid path")
                .build()
                .expect("valid project");
            let source_id = project
                .sources()
                .get_id(project.entry())
                .expect("source id");
            let result = super::compile(&project, &CompileOptions::default());
            let diagnostic = result
                .diagnostics
                .iter()
                .find(|diagnostic| diagnostic.code == "E8001")
                .unwrap_or_else(|| panic!("{entry}: expected E8001, got {:?}", result.diagnostics));
            let span = diagnostic.primary.expect("raw HTML primary span");
            assert_eq!(span.source_id, source_id);
            assert_eq!(
                inline_source.get(span.start..span.end),
                Some("<!-- note -->")
            );
        }

        let block_source = "<!-- note -->\n";
        for entry in [
            "main.md",
            "main.MD",
            "main.qd",
            "main.QD",
            "main.scrib",
            "main.SCRIB",
        ] {
            let project = VirtualProjectBuilder::new()
                .entry(entry)
                .expect("valid path")
                .add_source(entry, block_source)
                .expect("valid path")
                .build()
                .expect("valid project");
            let result = super::compile(&project, &CompileOptions::default());
            if entry.to_ascii_lowercase().ends_with(".md") {
                assert!(result.diagnostics.is_empty(), "{entry}: {result:?}");
                assert!(result.ir.nodes.is_empty(), "{entry}: {:?}", result.ir.nodes);
            } else {
                assert_eq!(result.diagnostics.len(), 1, "{entry}: {result:?}");
                assert_eq!(result.diagnostics[0].code, "E8001");
                let span = result.diagnostics[0].primary.expect("block HTML span");
                assert_eq!(block_source.get(span.start..span.end), Some(block_source));
            }
        }
    }

    #[test]
    fn compile_raw_html_semantics_follow_the_entry_source_mode() {
        let source = "before <em>one <strong>two</strong></em><br>after\n";
        for entry in ["main.md", "main.qd", "main.scrib"] {
            let project = VirtualProjectBuilder::new()
                .entry(entry)
                .expect("valid path")
                .add_source(entry, source)
                .expect("valid path")
                .build()
                .expect("valid project");
            let source_id = project.sources().get_id(project.entry()).unwrap();
            let result = super::compile(&project, &CompileOptions::default());
            let html_diagnostics = result
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "E8001")
                .collect::<Vec<_>>();

            if entry.ends_with(".md") {
                assert!(html_diagnostics.is_empty(), "{entry}: {result:?}");
                let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
                    panic!("{entry}: expected paragraph, got {:?}", result.ir.nodes);
                };
                let Some(IrInline::Emphasis {
                    content: emphasis_content,
                    ..
                }) = content
                    .iter()
                    .find(|inline| matches!(inline, IrInline::Emphasis { .. }))
                else {
                    panic!("{entry}: expected HTML emphasis, got {content:?}");
                };
                assert!(emphasis_content
                    .iter()
                    .any(|inline| matches!(inline, IrInline::Strong { .. })));
                assert!(content
                    .iter()
                    .any(|inline| matches!(inline, IrInline::HardBreak { .. })));
                assert!(content.iter().any(
                    |inline| matches!(inline, IrInline::Text { content, .. } if content == "before ")
                ));
                assert!(content.iter().any(
                    |inline| matches!(inline, IrInline::Text { content, .. } if content == "after")
                ));
            } else {
                assert_eq!(html_diagnostics.len(), 5, "{entry}: {result:?}");
                assert_eq!(
                    html_diagnostics
                        .iter()
                        .map(|diagnostic| {
                            let span = diagnostic.primary.expect("raw HTML primary span");
                            assert_eq!(span.source_id, source_id);
                            source[span.start..span.end].to_string()
                        })
                        .collect::<Vec<_>>(),
                    vec!["<em>", "<strong>", "</strong>", "</em>", "<br>"]
                );
                let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
                    panic!("{entry}: expected paragraph, got {:?}", result.ir.nodes);
                };
                assert!(content.iter().all(|inline| {
                    !matches!(
                        inline,
                        IrInline::Emphasis { .. }
                            | IrInline::Strong { .. }
                            | IrInline::Strikethrough { .. }
                            | IrInline::HardBreak { .. }
                    )
                }));
            }
        }
    }

    #[test]
    fn compile_raw_html_whitelist_is_markdown_only_for_all_supported_forms() {
        for source in [
            "<em>x</em>\n",
            "<strong>x</strong>\n",
            "<del>x</del>\n",
            "<s>x</s>\n",
            "before <br> after\n",
            "before <br/> after\n",
            "before <br /> after\n",
            "<EM>x</EM>\n",
            "<Strong>x</Strong>\n",
            "before <BR> after\n",
        ] {
            let markdown_project = VirtualProjectBuilder::new()
                .entry("main.md")
                .expect("valid path")
                .add_source("main.md", source)
                .expect("valid path")
                .build()
                .expect("valid project");
            let markdown_result = super::compile(&markdown_project, &CompileOptions::default());
            assert!(
                markdown_result.diagnostics.is_empty(),
                "Markdown source {source:?}: {:?}",
                markdown_result.diagnostics
            );
            let IrNode::Paragraph {
                content: markdown_content,
                ..
            } = &markdown_result.ir.nodes[0]
            else {
                panic!("Markdown source {source:?}: expected paragraph");
            };
            let source_lower = source.to_ascii_lowercase();
            if source_lower.contains("<em>") {
                assert!(markdown_content
                    .iter()
                    .any(|inline| matches!(inline, IrInline::Emphasis { .. })));
            }
            if source_lower.contains("<strong>") {
                assert!(markdown_content
                    .iter()
                    .any(|inline| matches!(inline, IrInline::Strong { .. })));
            }
            if source_lower.contains("<del>") || source_lower.contains("<s>") {
                assert!(markdown_content
                    .iter()
                    .any(|inline| matches!(inline, IrInline::Strikethrough { .. })));
            }
            if source_lower.contains("<br") {
                assert!(markdown_content
                    .iter()
                    .any(|inline| matches!(inline, IrInline::HardBreak { .. })));
            }

            for entry in ["main.qd", "main.scrib"] {
                let project = VirtualProjectBuilder::new()
                    .entry(entry)
                    .expect("valid path")
                    .add_source(entry, source)
                    .expect("valid path")
                    .build()
                    .expect("valid project");
                let result = super::compile(&project, &CompileOptions::default());
                assert!(
                    result
                        .diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.code == "E8001"),
                    "{entry} source {source:?}: {:?}",
                    result.diagnostics
                );
                assert!(result.ir.nodes.iter().all(|node| match node {
                    IrNode::Paragraph { content, .. } => content.iter().all(|inline| {
                        !matches!(
                            inline,
                            IrInline::Emphasis { .. }
                                | IrInline::Strong { .. }
                                | IrInline::Strikethrough { .. }
                                | IrInline::HardBreak { .. }
                        )
                    }),
                    _ => true,
                }));
            }
        }
    }

    #[test]
    fn compile_raw_html_diagnostics_preserve_utf8_crlf_source_spans_in_each_quarkdown_mode() {
        let source = "한글 <em>내용</em> 끝\r\n";
        for entry in ["main.qd", "main.scrib"] {
            let project = VirtualProjectBuilder::new()
                .entry(entry)
                .expect("valid path")
                .add_source(entry, source)
                .expect("valid path")
                .build()
                .expect("valid project");
            let source_id = project.sources().get_id(project.entry()).unwrap();
            let result = super::compile(&project, &CompileOptions::default());
            let diagnostics = result
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "E8001")
                .collect::<Vec<_>>();
            assert_eq!(diagnostics.len(), 2, "{entry}: {result:?}");
            for (diagnostic, expected) in diagnostics.iter().zip(["<em>", "</em>"]) {
                let span = diagnostic.primary.expect("raw HTML primary span");
                assert_eq!(span.source_id, source_id);
                assert!(span.start > 0);
                assert!(span.start < span.end);
                assert_eq!(source.get(span.start..span.end), Some(expected));
            }
        }
    }

    #[test]
    fn compile_block_raw_html_remains_source_backed_and_unsupported_in_each_mode() {
        let source = "<div>\r\n**not Markdown**\r\n</div>\r\n";
        for entry in ["main.md", "main.qd", "main.scrib"] {
            let project = VirtualProjectBuilder::new()
                .entry(entry)
                .expect("valid path")
                .add_source(entry, source)
                .expect("valid path")
                .build()
                .expect("valid project");
            let source_id = project.sources().get_id(project.entry()).unwrap();
            let result = super::compile(&project, &CompileOptions::default());
            assert_eq!(result.diagnostics.len(), 1, "{entry}: {result:?}");
            assert_eq!(result.diagnostics[0].code, "E8001");
            let span = result.diagnostics[0]
                .primary
                .expect("block raw HTML primary span");
            assert_eq!(span.source_id, source_id);
            assert_eq!(source.get(span.start..span.end), Some(source));
            assert!(result.ir.nodes.is_empty());
        }
    }

    #[test]
    fn compile_md_preserves_utf8_crlf_break_semantics_and_spans() {
        let source = "한글\r\n다음  \r\n끝";
        let project = VirtualProjectBuilder::new()
            .entry("main.md")
            .expect("valid path")
            .add_source("main.md", source)
            .expect("valid path")
            .build()
            .unwrap();
        let source_id = project.sources().get_id(project.entry()).unwrap();
        let result = super::compile(&project, &CompileOptions::default());
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!("expected paragraph")
        };
        match content.as_slice() {
            [IrInline::Text {
                content: first,
                span: first_span,
            }, IrInline::SoftBreak { span: soft_span }, IrInline::Text {
                content: second,
                span: second_span,
            }, IrInline::HardBreak { span: hard_span }, IrInline::Text {
                content: third,
                span: third_span,
            }] => {
                assert_eq!(first, "한글");
                assert_eq!(second, "다음");
                assert_eq!(third, "끝");
                assert_eq!(
                    *first_span,
                    scribium_source::SourceSpan::new(source_id, 0, 6)
                );
                assert_eq!(
                    *soft_span,
                    scribium_source::SourceSpan::new(source_id, 6, 8)
                );
                assert_eq!(
                    *second_span,
                    scribium_source::SourceSpan::new(source_id, 8, 14)
                );
                assert_eq!(
                    *hard_span,
                    scribium_source::SourceSpan::new(source_id, 14, 18)
                );
                assert_eq!(
                    *third_span,
                    scribium_source::SourceSpan::new(source_id, 18, 21)
                );
            }
            other => panic!("unexpected inline structure: {other:?}"),
        }
    }

    #[test]
    fn compile_evaluates_if_true() {
        let (result, _) = compile_source(".if {true}\n    hello\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        assert_eq!(content.len(), 1);
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn compile_evaluates_if_false() {
        let (result, _) = compile_source(".if {false}\n    dropped\n");
        assert!(result.diagnostics.is_empty());
        assert!(result.ir.nodes.is_empty());
    }

    #[test]
    fn compile_evaluates_ifnot() {
        let (result, _) = compile_source(".ifnot {no}\n    kept\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
    }

    #[test]
    fn compile_evaluates_nested_if() {
        let (result, _) =
            compile_source(".if {yes}\n    .if {no}\n        inner-dropped\n    inner-kept\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "inner-kept");
    }

    #[test]
    fn compile_reports_e3001_for_unresolvable_condition() {
        let (result, source_id) = compile_source(".if {maybe}\n    body\n");
        assert_eq!(result.diagnostics.len(), 1);
        let diag = &result.diagnostics[0];
        assert_eq!(diag.code, "E3001");
        assert!(matches!(diag.severity, Severity::Error));
        assert_eq!(diag.primary.as_ref().map(|s| s.source_id), Some(source_id));
        // If condition unknown -> false -> body dropped
        assert!(result.ir.nodes.is_empty());
    }

    #[test]
    fn compile_evaluates_named_condition_true() {
        let (result, _) = compile_source(".if condition:{true}\n    kept\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
    }

    #[test]
    fn compile_evaluates_named_condition_false() {
        let (result, _) = compile_source(".if condition:{false}\n    dropped\n");
        assert!(result.diagnostics.is_empty());
        assert!(result.ir.nodes.is_empty());
    }

    #[test]
    fn compile_evaluates_named_condition_yes_no() {
        let (result, _) = compile_source(".if condition:{yes}\n    kept\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);

        let (result, _) = compile_source(".ifnot condition:{no}\n    kept\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
    }

    #[test]
    fn compile_evaluates_named_body() {
        let (result, _) = compile_source(".if {true} body:{shown}\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "shown");
    }

    #[test]
    fn compile_evaluates_named_condition_and_body() {
        let (result, _) = compile_source(".if condition:{true} body:{shown}\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "shown");
    }

    #[test]
    fn compile_inline_named_condition() {
        let (result, _) = compile_source("before .if condition:{true} body:{inline} after\n");
        assert!(result.diagnostics.is_empty());
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let rendered: String = content
            .iter()
            .map(|i| match i {
                IrInline::Text { content, .. } => content.clone(),
                _ => String::new(),
            })
            .collect();
        assert!(rendered.contains("inline"));
    }

    #[test]
    fn compile_variable_declaration_and_reference() {
        let (result, _) = compile_source(".var {name} {Scribium}\nHello .name\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[1] else {
            panic!()
        };
        assert_eq!(text, "Scribium");
    }

    #[test]
    fn compile_variable_boolean_in_conditional() {
        let (result, _) = compile_source(".var {enabled} {yes}\n.if {.enabled}\n    visible\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "visible");
    }

    #[test]
    fn compile_variable_false_conditional() {
        let (result, _) = compile_source(".var {enabled} {no}\n.if {.enabled}\n    hidden\n");
        assert!(result.diagnostics.is_empty());
        assert!(result.ir.nodes.is_empty());
    }

    #[test]
    fn compile_variable_ifnot() {
        let (result, _) = compile_source(".var {enabled} {no}\n.ifnot {.enabled}\n    visible\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
    }

    #[test]
    fn compile_variable_explicit_reassignment() {
        let (result, _) = compile_source(".var {name} {A}\n.var {name} {B}\n.name\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "B");
    }

    #[test]
    fn compile_variable_name_reassignment() {
        let (result, _) = compile_source(".var {name} {A}\n.name\n.name {B}\n.name\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 2);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "A");
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[1] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "B");
    }

    #[test]
    fn compile_variable_inline_use() {
        let (result, _) = compile_source(".var {name} {world}\nHello **.name**\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let IrInline::Strong { content, .. } = &content[1] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "world");
    }

    #[test]
    fn compile_variable_block_variable() {
        let (result, _) = compile_source(".var {section}\n    # Title\n    body\n.section\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 2);
        let IrNode::Heading { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "Title");
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[1] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "body");
    }

    #[test]
    fn compile_variable_conditional_declaration() {
        let (result, _) = compile_source(".if {false}\n    .var {x} {hidden}\n.x\n");
        assert!(result.diagnostics.is_empty());
        // x not declared, preserved as function call
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::FunctionCall { name, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        assert_eq!(name, "x");
    }

    #[test]
    fn compile_variable_unknown_preserved() {
        let (result, _) = compile_source(".unknown\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::FunctionCall { name, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        assert_eq!(name, "unknown");
    }

    #[test]
    fn compile_variable_malformed_reports_e3002() {
        let (result, source_id) = compile_source(".var\n");
        assert_eq!(result.diagnostics.len(), 1);
        let diag = &result.diagnostics[0];
        assert_eq!(diag.code, "E3002");
        assert!(matches!(diag.severity, Severity::Error));
        assert_eq!(diag.primary.as_ref().map(|s| s.source_id), Some(source_id));
    }

    #[test]
    fn compile_variable_nested_in_block() {
        let (result, _) =
            compile_source(".var {section}\n    .if {true}\n        nested\n.section\n");
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "nested");
    }

    #[test]
    fn compile_variable_immutable_and_deterministic() {
        let source = ".var {name} {A}\n.name\n";
        let project1 = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", source)
            .expect("valid path")
            .build()
            .unwrap();
        let project2 = VirtualProjectBuilder::new()
            .entry("main.qd")
            .expect("valid path")
            .add_source("main.qd", source)
            .expect("valid path")
            .build()
            .unwrap();
        let result1 = super::compile(&project1, &CompileOptions::default());
        let result2 = super::compile(&project2, &CompileOptions::default());
        assert_eq!(result1.ir, result2.ir);
    }

    #[test]
    fn compile_variable_rich_content_block_reference() {
        // Rushdown exposes no original-source inline-fragment parser for this
        // content span. Preserve the source and report the unsupported gap;
        // do not synthesize a Markdown document or claim Strong semantics.
        let (result, _) = compile_source(".var {x} {**hello**}\n.x\n");
        assert!(result.diagnostics.iter().any(|diag| diag.code == "E3010"));
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!("expected paragraph, got {:?}", result.ir.nodes[0])
        };
        assert!(content
            .iter()
            .all(|inline| !matches!(inline, IrInline::Strong { .. })));
    }

    #[test]
    fn compile_variable_rich_content_inline_reference() {
        // The same original-source-only limitation applies to inline variable
        // expansion. The unsupported diagnostic prevents silent data loss.
        let (result, _) = compile_source(".var {x} {**world**}\nHello .x\n");
        assert!(result.diagnostics.iter().any(|diag| diag.code == "E3010"));
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &result.ir.nodes[0] else {
            panic!()
        };
        assert!(content
            .iter()
            .all(|inline| !matches!(inline, IrInline::Strong { .. })));
    }

    #[test]
    fn compile_variable_multiple_paragraphs_inline_reference_is_not_flattened() {
        let source = ".var {x}\n    First\n\n    Second\n\nprefix .x suffix\n";
        let (result, _) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].code, "E3003");
        assert!(!output_text(&result).contains("First"));
        assert!(!output_text(&result).contains("Second"));
    }

    #[test]
    fn compile_variable_invalid_name_reports_e3002() {
        // .var {"bad name"} {hello} should report E3002
        let (result, source_id) = compile_source(r#".var {"bad name"} {hello}"#);
        assert_eq!(result.diagnostics.len(), 1);
        let diag = &result.diagnostics[0];
        assert_eq!(diag.code, "E3002");
        assert!(matches!(diag.severity, Severity::Error));
        assert!(diag.message.contains("Invalid variable name"));
        assert_eq!(diag.primary.as_ref().map(|s| s.source_id), Some(source_id));
    }

    #[test]
    fn compile_variable_reference_with_body_preserved_as_call() {
        // .var {foo} {value} / .foo { body } should preserve the call with body
        let (result, _) = compile_source(".var {foo} {value}\n.foo\n    body\n");
        assert!(
            result.diagnostics.is_empty(),
            "diagnostics: {:?}",
            result.diagnostics
        );
        // Should be preserved as function call, not variable reference
        assert_eq!(result.ir.nodes.len(), 1);
        let IrNode::FunctionCall {
            name,
            body: call_body,
            ..
        } = &result.ir.nodes[0]
        else {
            panic!("expected function call, got {:?}", result.ir.nodes[0])
        };
        assert_eq!(name, "foo");
        assert!(call_body.is_some());
        let body_nodes = call_body.as_ref().unwrap();
        assert_eq!(body_nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &body_nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "body");
    }

    #[test]
    fn compile_logical_comparisons_drive_conditionals_and_nested_calls() {
        let source = "\
.var {value} {2}
.if {.islower {.value} than:{3}}
    below
.ifnot {.isgreater {.value} than:{3}}
    not-greater
.if {.isgreater {3} than:{3} orequals:{yes}}
    inclusive
.if {.equals {2} to:{\"2\"}}
    equal
.if {.not {.equals {2} to:{3}}}
    distinct
";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(
            output_text(&result),
            "below\nnot-greater\ninclusive\nequal\ndistinct"
        );
    }

    #[test]
    fn compile_logical_comparisons_work_in_user_functions_and_chains() {
        let source = "\
.function {classify}
    value:
    .if {.value::islower than:{3}}
        small
    .ifnot {.value::islower than:{3}}
        large

.classify {2}
.classify {4}
";
        let (result, _) = compile_source(source);
        assert!(result.diagnostics.is_empty(), "{result:?}");
        assert_eq!(output_text(&result), "small\nlarge");
    }

    #[test]
    fn compile_logical_comparison_failure_is_atomic_and_source_backed() {
        let source = "앞 문장\r\n.if {.islower {not-a-number} than:{3}}\r\n    숨겨진 내용\r\n";
        let (result, source_id) = compile_source(source);
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        let diagnostic = &result.diagnostics[0];
        assert_eq!(diagnostic.code, "E3001");
        let comparison_start = source.find(".islower").expect("comparison call");
        let comparison_end = comparison_start + ".islower {not-a-number} than:{3}".len();
        assert_eq!(
            diagnostic.primary,
            Some(scribium_source::SourceSpan::new(
                source_id,
                comparison_start,
                comparison_end,
            ))
        );
        assert_eq!(output_text(&result), "앞 문장");
        assert!(!output_text(&result).contains("숨겨진 내용"));
    }

    #[test]
    fn compile_logical_comparison_execution_is_deterministic_for_utf8_crlf() {
        let source = "한글\r\n.if {.equals {값} to:{값}}\r\n    통과\r\n";
        let first = compile_source(source).0;
        let second = compile_source(source).0;
        assert_eq!(first.ir, second.ir);
        assert_eq!(
            serde_json::to_string(&first.diagnostics).expect("diagnostics serialize"),
            serde_json::to_string(&second.diagnostics).expect("diagnostics serialize")
        );
        assert!(first.diagnostics.is_empty(), "{first:?}");
        assert_eq!(output_text(&first), "한글\n통과");
    }
}
