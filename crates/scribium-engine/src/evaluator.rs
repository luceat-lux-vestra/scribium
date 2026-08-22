//! M1/M2 evaluator: resolves Quarkdown conditionals, variables, scoped `.let`
//! calls, user-defined functions, and the first value-flow builtins used by
//! `::` call chains.
//!
//! Evaluation runs after parsing and `ast_to_ir` and before Typst lowering.
//! It operates on the IR: a `FunctionCall` / `DirectiveCall` named `if` or
//! `ifnot` is replaced by its content when its boolean condition holds,
//! otherwise it is removed (Quarkdown conditional-statements semantics,
//! wiki badged `v2.5.0`, accessed 2026-08-08).
//!
//! The condition is the first positional argument and must be one of the
//! boolean literals documented for the Quarkdown Boolean value type:
//! `true` / `yes` for true and `false` / `no` for false, case-insensitive.
//! Any other condition (or a missing one) is reported with the `E3001`
//! evaluation error and the construct is treated as false, keeping output
//! deterministic.
//!
//! The content of a conditional is, in order of priority: the indented
//! block body, the second positional argument when it is a content value,
//! or a bare scalar argument rendered as text.
//!
//! Variable evaluation (`.var`) follows Quarkdown document-scope variable
//! semantics: declarations create bindings in a document-wide environment;
//! parameterless calls (`.name`) resolve to the bound value; reassignment
//! is supported via explicit `.var {name} {value}` or variable-name
//! call `.name {value}` (only if `name` is already a variable). Unknown
//! parameterless calls are preserved as function calls, not variable errors.
//!
//! Block variables (`.var {name}\n    body\n.name`) store evaluated content
//! and materialize it at reference sites.
//!
//! User-defined functions are registered in source order. A call evaluates
//! positional and named arguments first, creates a child scope, binds its
//! parameters, and then evaluates the body statement-by-statement in value
//! context. Outputless statements update the child scope, one substantive
//! semantic value remains an `IrValue` at that boundary, and multiple
//! structured outputs become `IrValue::Content` only when composition requires
//! it.
//!
//! Chain evaluation is structural: the head is invoked first, its semantic
//! `IrValue` becomes the first positional argument of the next segment, and
//! segments continue in source order. No source or backend text is generated
//! during this process.

use crate::value_conversion::{
    self, InvocationNamedArg, InvocationValue, ScalarTarget, ScalarValue, ValueOrigin,
};
use crate::{ast_to_ir, builtins};
use crate::{
    Capabilities, Capability, EvaluationLimits, IncludedSource, ResourceAccessError,
    ResourceProvider, ResourceText,
};
use scribium_diagnostics::{Diagnostic, Severity};
use scribium_ir::{
    IrCallSegment, IrCallable, IrCallableCapture, IrCapturedFunction, IrCapturedVariable,
    IrComponent, IrContainerAlignment, IrContainerComponent, IrCrossAxisAlignment, IrDictionary,
    IrDocument, IrEnumValue, IrInline, IrLandscapeComponent, IrListItem, IrMainAxisAlignment,
    IrNamedArg, IrNode, IrPair, IrParameter, IrRange, IrSize, IrSizeUnit, IrStackedComponent,
    IrStackedLayout, IrTableAlignment, IrTableCell, IrTableRow, IrValue, NativeTarget,
    TargetSpecificContent,
};
use scribium_markdown::Mode;
use scribium_quarkdown::is_valid_normal_call_name;
use scribium_source::{SourceId, SourceSpan};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::rc::Rc;

/// A resolved variable value stored in the evaluation environment.
///
/// Variables can hold any IR value type. The value is fully evaluated
/// at declaration time (for scalars) or stored as content nodes for
/// block variables.
#[derive(Debug, Clone)]
enum VariableValue {
    Scalar(IrValue),
    Content(Vec<IrNode>),
}

impl VariableValue {
    /// Creates a VariableValue from an evaluated IrValue, preserving content semantics.
    fn from_evaluated_value(value: IrValue) -> Self {
        match value {
            IrValue::Content(nodes) => VariableValue::Content(nodes),
            scalar => VariableValue::Scalar(scalar),
        }
    }

    /// Returns the backend-neutral value used when this variable participates
    /// in a chain.
    fn to_value(&self) -> IrValue {
        match self {
            VariableValue::Scalar(value) => value.clone(),
            VariableValue::Content(nodes) => IrValue::Content(nodes.clone()),
        }
    }
}

/// A source-backed callable definition stored in an evaluator scope.
#[derive(Debug, Clone, PartialEq)]
struct FunctionBinding {
    parameters: LambdaParameters,
    body: Vec<IrNode>,
    declaration_span: SourceSpan,
    capture: Option<Box<IrCallableCapture>>,
}

impl FunctionBinding {
    fn as_callable(&self) -> IrCallable {
        IrCallable {
            parameters: self.parameters.to_ir(),
            body: self.body.clone(),
            span: self.declaration_span,
            capture: self.capture.clone(),
        }
    }
}

/// The parameter mode of a callable body.
///
/// Explicit parameters retain their source-backed names and optionality.
/// Headerless lambdas expose the invocation's positional values through the
/// invocation-local implicit scope. Keeping this distinction in the callable
/// representation lets both modes use the same argument evaluation and body
/// invocation path without aliasing `.1` onto an explicit parameter.
#[derive(Debug, Clone, PartialEq)]
enum LambdaParameters {
    Explicit(Vec<IrParameter>),
    Implicit,
}

impl LambdaParameters {
    #[cfg(test)]
    fn explicit(&self) -> Option<&[IrParameter]> {
        match self {
            Self::Explicit(parameters) => Some(parameters),
            Self::Implicit => None,
        }
    }

    fn description(&self) -> String {
        match self {
            Self::Explicit(parameters) => format!("{} explicit parameter(s)", parameters.len()),
            Self::Implicit => "implicit positional parameters".to_string(),
        }
    }

    fn to_ir(&self) -> Option<Vec<IrParameter>> {
        match self {
            Self::Explicit(parameters) => Some(parameters.clone()),
            Self::Implicit => None,
        }
    }

    fn from_ir(parameters: Option<Vec<IrParameter>>) -> Self {
        parameters.map_or(Self::Implicit, Self::Explicit)
    }
}

enum BoundLambdaArguments {
    Explicit(Vec<IrValue>),
    Implicit(Vec<IrValue>),
}

/// The implicit-parameter boundary installed for one callable invocation.
///
/// An explicit invocation deliberately masks any outer implicit scope. This
/// prevents `.1` in an explicit lambda from accidentally capturing an outer
/// lambda's argument.
#[derive(Debug, Clone, PartialEq)]
enum LambdaScope {
    Explicit,
    Implicit(Vec<IrValue>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImplicitParameterIndex {
    Valid(usize),
    Overflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImplicitParameterError {
    Missing,
    Overflow,
}

/// A call body that has not been evaluated yet.
///
/// Keeping the source body in this form lets the callee decide whether it is
/// eager or lazy. In particular, conditionals must inspect their condition
/// before evaluating an unreachable body.
#[derive(Clone, Copy)]
enum CallBody<'a> {
    Block(&'a [IrNode]),
    Inline(&'a [IrInline]),
}

#[derive(Clone)]
struct StackedArgument {
    value: InvocationValue,
    span: SourceSpan,
}

struct BoundStackedArguments {
    values: Vec<Option<StackedArgument>>,
}

#[derive(Clone)]
struct AlignArgument {
    value: InvocationValue,
    span: SourceSpan,
}

#[derive(Clone)]
struct ContainerArgument {
    value: InvocationValue,
    span: SourceSpan,
}

#[derive(Clone)]
struct WhitespaceArgument {
    value: InvocationValue,
    span: SourceSpan,
}

struct BoundContainerArguments {
    width: Option<ContainerArgument>,
    height: Option<ContainerArgument>,
    full_width: Option<ContainerArgument>,
}

struct BoundWhitespaceArguments {
    width: Option<WhitespaceArgument>,
    height: Option<WhitespaceArgument>,
}

impl BoundStackedArguments {
    fn take(&mut self, index: usize) -> Option<StackedArgument> {
        self.values.get_mut(index).and_then(Option::take)
    }
}

#[derive(Clone, Copy)]
struct IterationOptions {
    span: SourceSpan,
    allow_destructuring: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum SortKey {
    Number(f64),
    String(String),
    Boolean(bool),
}

impl SortKey {
    fn try_from_value(value: &IrValue) -> Result<Self, String> {
        match value {
            IrValue::Number(value) => Ok(Self::Number(*value)),
            IrValue::String(value) => Ok(Self::String(value.clone())),
            IrValue::Boolean(value) => Ok(Self::Boolean(*value)),
            IrValue::None => Err("`.sorted` cannot compare a None value".to_string()),
            _ => Err("`.sorted` key has no supported natural ordering".to_string()),
        }
    }

    fn same_kind(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Number(_), Self::Number(_))
                | (Self::String(_), Self::String(_))
                | (Self::Boolean(_), Self::Boolean(_))
        )
    }
}

impl Eq for SortKey {}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(left), Self::Number(right)) => match (left.is_nan(), right.is_nan()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => left.total_cmp(right),
            },
            (Self::String(left), Self::String(right)) => left.cmp(right),
            (Self::Boolean(left), Self::Boolean(right)) => left.cmp(right),
            _ => Ordering::Equal,
        }
    }
}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Result of invoking a call in value context.
///
/// `Unresolved` is distinct from an empty content value: an ordinary output
/// context may preserve it, while a chain must reject it because it cannot
/// inject a fabricated intermediate value.
#[derive(Debug, PartialEq)]
enum CallOutcome {
    Value(IrValue),
    NoValue,
    Failed,
    Unresolved,
}

/// Mutable evaluator-only document state. Its final form is copied into the
/// serializable IR snapshot after evaluation completes.
#[derive(Debug, Clone, Default)]
struct DocumentState {
    name: String,
    description: String,
    document_type: scribium_ir::IrDocumentType,
}

impl DocumentState {
    fn from_snapshot(snapshot: &scribium_ir::IrDocumentState) -> Self {
        Self {
            name: snapshot.name.clone(),
            description: snapshot.description.clone(),
            document_type: snapshot.document_type,
        }
    }

    fn snapshot(&self) -> scribium_ir::IrDocumentState {
        scribium_ir::IrDocumentState {
            name: self.name.clone(),
            description: self.description.clone(),
            document_type: self.document_type,
        }
    }
}

/// Accumulates the observable result of a callable body without converting a
/// semantic value to document content until a second observable output makes
/// that conversion necessary.
enum CallableBodyValueAccumulator {
    Empty,
    Semantic { value: IrValue, span: SourceSpan },
    Content(Vec<IrNode>),
}

impl CallableBodyValueAccumulator {
    fn append_value(
        &mut self,
        value: IrValue,
        span: SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<(), CallOutcome> {
        if matches!(self, Self::Empty) {
            *self = Self::Semantic { value, span };
            return Ok(());
        }

        let current = std::mem::replace(self, Self::Empty);
        let mut nodes = current.into_content_nodes(diagnostics)?;
        nodes.extend(value_into_content_nodes(value, span, diagnostics)?);
        *self = Self::Content(nodes);
        Ok(())
    }

    fn finish(self) -> CallOutcome {
        match self {
            Self::Empty => CallOutcome::NoValue,
            Self::Semantic { value, .. } => CallOutcome::Value(value),
            Self::Content(nodes) => CallOutcome::Value(IrValue::Content(nodes)),
        }
    }

    fn into_content_nodes(
        self,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Vec<IrNode>, CallOutcome> {
        match self {
            Self::Empty => Ok(Vec::new()),
            Self::Semantic { value, span } => value_into_content_nodes(value, span, diagnostics),
            Self::Content(nodes) => Ok(nodes),
        }
    }
}

fn value_into_content_nodes(
    value: IrValue,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<IrNode>, CallOutcome> {
    match value {
        IrValue::Content(nodes) => Ok(nodes),
        IrValue::Collection(values) => {
            let mut nodes = Vec::new();
            if let Err(error) = nodes.try_reserve(values.len()) {
                diagnostics.push(iteration_error(
                    format!("collection content cannot be allocated: {error}"),
                    span,
                ));
                return Err(CallOutcome::Failed);
            }
            for value in values {
                nodes.extend(value_into_content_nodes(value, span, diagnostics)?);
            }
            Ok(nodes)
        }
        IrValue::Pair(pair) => pair_into_content_nodes(pair, diagnostics),
        IrValue::Dictionary(dictionary) => {
            dictionary_into_table(dictionary, diagnostics).map(|table| vec![table])
        }
        IrValue::Component(component) => Ok(vec![IrNode::Component { component }]),
        IrValue::Range(range) => {
            diagnostics.push(iteration_error(
                "Direct Range materialization is deferred; consume the typed Range through iteration first"
                    .to_string(),
                range.span,
            ));
            Err(CallOutcome::Failed)
        }
        scalar => match scalar_to_text(&scalar, span, diagnostics) {
            Ok(content) => Ok(vec![IrNode::Paragraph {
                content: vec![IrInline::Text { content, span }],
                span,
            }]),
            Err(outcome) => Err(outcome),
        },
    }
}

fn pair_into_content_nodes(
    pair: IrPair,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<IrNode>, CallOutcome> {
    let mut items = Vec::new();
    if let Err(error) = items.try_reserve_exact(2) {
        diagnostics.push(iteration_error(
            format!("pair output collection cannot be allocated: {error}"),
            pair.span,
        ));
        return Err(CallOutcome::Failed);
    }
    for value in [*pair.first, *pair.second] {
        let nodes = value_into_content_nodes(value, pair.span, diagnostics)?;
        items.push(IrListItem {
            nodes,
            task: None,
            span: pair.span,
        });
    }
    Ok(vec![IrNode::OrderedList {
        items,
        start: 1,
        span: pair.span,
    }])
}

fn dictionary_into_table(
    dictionary: IrDictionary,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<IrNode, CallOutcome> {
    let span = dictionary.span;
    let header = IrTableRow {
        cells: vec![table_text_cell("Key", span), table_text_cell("Value", span)],
        span,
    };
    let mut rows = Vec::new();
    if let Err(error) = rows.try_reserve_exact(dictionary.entries.len()) {
        diagnostics.push(iteration_error(
            format!("dictionary output table cannot be allocated: {error}"),
            span,
        ));
        return Err(CallOutcome::Failed);
    }
    for pair in dictionary.entries {
        let IrPair {
            first,
            second,
            span: pair_span,
        } = pair;
        let IrValue::String(key) = *first else {
            diagnostics.push(iteration_error(
                "Dictionary keys must remain typed strings".to_string(),
                pair_span,
            ));
            return Err(CallOutcome::Failed);
        };
        let value = value_into_table_cell(*second, pair_span, diagnostics)?;
        rows.push(IrTableRow {
            cells: vec![
                table_text_cell(&key, pair_span),
                IrTableCell {
                    content: value,
                    alignment: IrTableAlignment::None,
                    span: pair_span,
                },
            ],
            span: pair_span,
        });
    }
    Ok(IrNode::Table { header, rows, span })
}

fn table_text_cell(content: &str, span: SourceSpan) -> IrTableCell {
    IrTableCell {
        content: vec![IrInline::Text {
            content: content.to_string(),
            span,
        }],
        alignment: IrTableAlignment::None,
        span,
    }
}

fn value_into_table_cell(
    value: IrValue,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<IrInline>, CallOutcome> {
    match value {
        IrValue::Content(nodes) => match nodes.as_slice() {
            [IrNode::Paragraph { content, .. }] => Ok(content.clone()),
            _ => {
                diagnostics.push(iteration_error(
                    "Dictionary values must be scalar or exactly one paragraph when rendered as a table cell"
                        .to_string(),
                    span,
                ));
                Err(CallOutcome::Failed)
            }
        },
        IrValue::Collection(values) => {
            let nodes = value_into_content_nodes(IrValue::Collection(values), span, diagnostics)?;
            match nodes.as_slice() {
                [IrNode::Paragraph { content, .. }] => Ok(content.clone()),
                _ => {
                    diagnostics.push(iteration_error(
                        "A multi-value Collection cannot be rendered as one Dictionary table cell"
                            .to_string(),
                        span,
                    ));
                    Err(CallOutcome::Failed)
                }
            }
        }
        IrValue::Pair(_) | IrValue::Dictionary(_) | IrValue::Range(_) => {
            diagnostics.push(iteration_error(
                "Nested Pair, Dictionary, or Range values cannot be rendered as one Dictionary table cell"
                    .to_string(),
                span,
            ));
            Err(CallOutcome::Failed)
        }
        scalar => match scalar_to_text(&scalar, span, diagnostics) {
            Ok(content) => Ok(vec![IrInline::Text { content, span }]),
            Err(outcome) => Err(outcome),
        },
    }
}

fn ir_node_source_span(node: &IrNode) -> SourceSpan {
    match node {
        IrNode::Heading { span, .. }
        | IrNode::Paragraph { span, .. }
        | IrNode::Blockquote { span, .. }
        | IrNode::UnorderedList { span, .. }
        | IrNode::OrderedList { span, .. }
        | IrNode::Table { span, .. }
        | IrNode::CodeBlock { span, .. }
        | IrNode::RawHtml { span, .. }
        | IrNode::FunctionCall { span, .. }
        | IrNode::ChainedFunctionCall { span, .. }
        | IrNode::FunctionDeclaration { span, .. }
        | IrNode::ThematicBreak { span }
        | IrNode::Math { span, .. } => *span,
        IrNode::TargetSpecificContent { content } => content.span,
        IrNode::Component { component } => component.span(),
    }
}

#[derive(Debug, Default)]
struct EvaluationRuntime {
    active_evaluation_depth: usize,
}

/// Releases one active evaluator frame even when evaluation returns early.
struct EvaluationDepthGuard {
    runtime: Rc<RefCell<EvaluationRuntime>>,
}

impl Drop for EvaluationDepthGuard {
    fn drop(&mut self) {
        let mut runtime = self.runtime.borrow_mut();
        debug_assert!(runtime.active_evaluation_depth > 0);
        runtime.active_evaluation_depth = runtime.active_evaluation_depth.saturating_sub(1);
    }
}

/// Evaluation context with explicit parent visibility and local bindings.
///
/// Created fresh per `evaluate()` call to ensure isolation and determinism.
/// Lookups walk the parent chain without cloning it. A child scope snapshots
/// the visible parent context at creation time and local writes stay in the
/// child. The snapshot is deliberate: a lambda observes the bindings visible
/// when it is entered, while its local declarations cannot leak back.
#[derive(Clone)]
struct EvaluationContext<'a> {
    parent: Option<Box<EvaluationContext<'a>>>,
    variables: BTreeMap<String, VariableValue>,
    functions: BTreeMap<String, FunctionBinding>,
    lambda_scope: Option<LambdaScope>,
    resources: Option<&'a dyn ResourceProvider>,
    metadata_defaults: crate::DocumentMetadataDefaults,
    current_source: Option<SourceId>,
    active_sources: Vec<SourceId>,
    document_state: Rc<RefCell<DocumentState>>,
    limits: EvaluationLimits,
    runtime: Rc<RefCell<EvaluationRuntime>>,
}

impl<'a> EvaluationContext<'a> {
    fn new() -> Self {
        Self::with_limits(EvaluationLimits::default())
    }

    fn with_limits(limits: EvaluationLimits) -> Self {
        Self {
            parent: None,
            variables: BTreeMap::new(),
            functions: BTreeMap::new(),
            lambda_scope: None,
            resources: None,
            metadata_defaults: crate::DocumentMetadataDefaults::default(),
            current_source: None,
            active_sources: Vec::new(),
            document_state: Rc::new(RefCell::new(DocumentState::default())),
            limits,
            runtime: Rc::new(RefCell::new(EvaluationRuntime::default())),
        }
    }

    /// Creates a child scope with parent-visible bindings and isolated locals.
    #[allow(dead_code)]
    fn child(&self) -> Self {
        Self {
            parent: Some(Box::new(self.clone())),
            variables: BTreeMap::new(),
            functions: BTreeMap::new(),
            lambda_scope: None,
            resources: self.resources,
            metadata_defaults: self.metadata_defaults.clone(),
            current_source: self.current_source,
            active_sources: self.active_sources.clone(),
            document_state: Rc::clone(&self.document_state),
            limits: self.limits,
            runtime: Rc::clone(&self.runtime),
        }
    }

    fn with_resources(
        resources: &'a dyn ResourceProvider,
        source_id: SourceId,
        metadata_defaults: &crate::DocumentMetadataDefaults,
        limits: EvaluationLimits,
    ) -> Self {
        Self {
            resources: Some(resources),
            metadata_defaults: metadata_defaults.clone(),
            current_source: Some(source_id),
            active_sources: vec![source_id],
            ..Self::with_limits(limits)
        }
    }

    fn enter_evaluation_depth(
        &self,
        span: SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<EvaluationDepthGuard, CallOutcome> {
        let mut runtime = self.runtime.borrow_mut();
        if runtime.active_evaluation_depth >= self.limits.max_evaluation_depth {
            diagnostics.push(evaluation_depth_limit_error(
                self.limits.max_evaluation_depth,
                span,
            ));
            return Err(CallOutcome::Failed);
        }
        runtime.active_evaluation_depth += 1;
        drop(runtime);
        Ok(EvaluationDepthGuard {
            runtime: Rc::clone(&self.runtime),
        })
    }

    /// Declares or reassigns a variable from an evaluated IrValue, preserving content semantics.
    fn set_value(&mut self, name: String, value: IrValue) {
        self.variables
            .insert(name, VariableValue::from_evaluated_value(value));
    }

    /// Installs a user-function binding in the current local scope.
    fn set_function_binding(
        &mut self,
        name: String,
        parameters: LambdaParameters,
        body: Vec<IrNode>,
        declaration_span: SourceSpan,
        capture: Option<Box<IrCallableCapture>>,
    ) {
        self.functions.insert(
            name,
            FunctionBinding {
                parameters,
                body,
                declaration_span,
                capture,
            },
        );
    }

    fn capture_snapshot(&self) -> IrCallableCapture {
        let mut variables = BTreeMap::new();
        let mut functions = BTreeMap::new();
        self.collect_bindings(&mut variables, &mut functions);
        IrCallableCapture {
            variables: variables
                .into_iter()
                .map(|(name, value)| IrCapturedVariable { name, value })
                .collect(),
            functions: functions
                .into_iter()
                .map(|(name, binding)| IrCapturedFunction {
                    name,
                    callable: binding.as_callable(),
                })
                .collect(),
        }
    }

    fn collect_bindings(
        &self,
        variables: &mut BTreeMap<String, IrValue>,
        functions: &mut BTreeMap<String, FunctionBinding>,
    ) {
        if let Some(parent) = self.parent.as_deref() {
            parent.collect_bindings(variables, functions);
        }
        variables.extend(
            self.variables
                .iter()
                .map(|(name, value)| (name.clone(), value.to_value())),
        );
        functions.extend(self.functions.clone());
    }

    fn from_capture(capture: &IrCallableCapture) -> Self {
        let mut context = Self::new();
        for variable in &capture.variables {
            context.set_value(variable.name.clone(), variable.value.clone());
        }
        for function in &capture.functions {
            context.functions.insert(
                function.name.clone(),
                FunctionBinding {
                    parameters: LambdaParameters::from_ir(function.callable.parameters.clone()),
                    body: function.callable.body.clone(),
                    declaration_span: function.callable.span,
                    capture: function.callable.capture.clone(),
                },
            );
        }
        context
    }

    /// Composes a callable's definition environment with the bindings visible
    /// at its call site. The definition context remains the parent layer, so
    /// caller-visible variables/functions supplement it without replacing the
    /// lexical capture or becoming part of that capture.
    fn with_caller_overlay(definition_context: Self, caller_context: &Self) -> Self {
        let mut variables = BTreeMap::new();
        let mut functions = BTreeMap::new();
        caller_context.collect_bindings(&mut variables, &mut functions);

        Self {
            parent: Some(Box::new(definition_context)),
            variables: variables
                .into_iter()
                .map(|(name, value)| (name, VariableValue::from_evaluated_value(value)))
                .collect(),
            functions,
            lambda_scope: caller_context.visible_lambda_scope(),
            // Runtime/compiler state is intentionally not copied into this
            // lookup-only layer. Document state is the one explicit shared
            // exception required by the document-state contract.
            resources: None,
            metadata_defaults: Default::default(),
            current_source: None,
            active_sources: Vec::new(),
            document_state: Rc::clone(&caller_context.document_state),
            limits: caller_context.limits,
            runtime: Rc::clone(&caller_context.runtime),
        }
    }

    #[cfg(test)]
    fn set_function(&mut self, name: String, parameters: Vec<String>) {
        let parameters = parameters
            .into_iter()
            .map(|name| IrParameter {
                name,
                name_span: SourceSpan::new(scribium_source::SourceId(0), 0, 0),
                span: SourceSpan::new(scribium_source::SourceId(0), 0, 0),
                optional: false,
            })
            .collect();
        self.set_function_binding(
            name,
            LambdaParameters::Explicit(parameters),
            Vec::new(),
            SourceSpan::new(scribium_source::SourceId(0), 0, 0),
            None,
        );
    }

    fn set_lambda_scope(&mut self, scope: LambdaScope) {
        self.lambda_scope = Some(scope);
    }

    fn initialize_document_state(&mut self, snapshot: &scribium_ir::IrDocumentState) {
        self.document_state = Rc::new(RefCell::new(DocumentState::from_snapshot(snapshot)));
    }

    fn document_state_snapshot(&self) -> scribium_ir::IrDocumentState {
        self.document_state.borrow().snapshot()
    }

    fn document_state_value(&self, name: &str) -> IrValue {
        let state = self.document_state.borrow();
        match name {
            "docname" => IrValue::String(state.name.clone()),
            "docdescription" => IrValue::String(state.description.clone()),
            "doctype" => IrValue::String(state.document_type.quarkdown_name().to_string()),
            _ => unreachable!("document state field must be validated by the caller"),
        }
    }

    fn set_document_state_value(&self, name: &str, value: String) {
        let mut state = self.document_state.borrow_mut();
        match name {
            "docname" => state.name = value,
            "docdescription" => state.description = value,
            _ => unreachable!("document state field must be validated by the caller"),
        }
    }

    fn set_document_type(&self, value: scribium_ir::IrDocumentType) {
        self.document_state.borrow_mut().document_type = value;
    }

    /// Gets a variable value if it exists.
    fn get(&self, name: &str) -> Option<&VariableValue> {
        self.variables
            .get(name)
            .or_else(|| self.parent.as_deref().and_then(|parent| parent.get(name)))
    }

    /// Checks if a name is bound as a variable.
    fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Looks up a function binding through the visible scope chain.
    fn get_function(&self, name: &str) -> Option<&FunctionBinding> {
        self.functions.get(name).or_else(|| {
            self.parent
                .as_deref()
                .and_then(|parent| parent.get_function(name))
        })
    }

    /// Resolves a numeric implicit parameter only inside the nearest lambda
    /// invocation. Explicit lambda scopes are a hard boundary: numeric
    /// references are diagnosed locally instead of falling through to an
    /// outer implicit invocation.
    fn get_implicit_parameter(
        &self,
        name: &str,
    ) -> Option<Result<IrValue, ImplicitParameterError>> {
        let index = implicit_parameter_index(name)?;
        match self.lambda_scope.as_ref() {
            Some(LambdaScope::Explicit) => Some(Err(ImplicitParameterError::Missing)),
            Some(LambdaScope::Implicit(arguments)) => match index {
                ImplicitParameterIndex::Valid(index) => {
                    let resolved = arguments
                        .get(index.saturating_sub(1))
                        .cloned()
                        .map(Ok)
                        .or_else(|| {
                            self.parent
                                .as_deref()
                                .and_then(|parent| parent.get_implicit_parameter(name))
                        });
                    Some(resolved.unwrap_or(Err(ImplicitParameterError::Missing)))
                }
                ImplicitParameterIndex::Overflow => Some(Err(ImplicitParameterError::Overflow)),
            },
            None => self
                .parent
                .as_deref()
                .and_then(|parent| parent.get_implicit_parameter(name)),
        }
    }

    /// Returns the nearest lambda scope that is visible from this context.
    /// The caller overlay copies this one scope as lookup state; it does not
    /// retain a reference to the mutable caller context.
    fn visible_lambda_scope(&self) -> Option<LambdaScope> {
        self.lambda_scope.clone().or_else(|| {
            self.parent
                .as_deref()
                .and_then(|parent| parent.visible_lambda_scope())
        })
    }
}

/// Evaluates Quarkdown conditionals, variables, user-defined functions, and
/// the currently supported semantic chain builtins in the IR.
#[derive(Debug, Clone, Copy)]
pub struct Evaluator {
    capabilities: Capabilities,
    limits: EvaluationLimits,
}

impl Default for Evaluator {
    fn default() -> Self {
        Self::new()
    }
}

impl Evaluator {
    /// Creates a new evaluator.
    pub fn new() -> Self {
        Self::with_capabilities_and_limits(Capabilities::default(), EvaluationLimits::default())
    }

    /// Creates an evaluator with the explicit capabilities for one compile.
    pub fn with_capabilities(capabilities: Capabilities) -> Self {
        Self::with_capabilities_and_limits(capabilities, EvaluationLimits::default())
    }

    /// Creates an evaluator with explicit semantic resource limits.
    pub fn with_limits(limits: EvaluationLimits) -> Self {
        Self::with_capabilities_and_limits(Capabilities::default(), limits)
    }

    /// Creates an evaluator with explicit capabilities and semantic resource
    /// limits for one compilation.
    pub fn with_capabilities_and_limits(
        capabilities: Capabilities,
        limits: EvaluationLimits,
    ) -> Self {
        Self {
            capabilities,
            limits,
        }
    }

    /// Evaluates the document, resolving conditionals, variables, and chains.
    ///
    /// Returns the resolved document and any evaluation diagnostics.
    pub fn evaluate(&self, document: &IrDocument) -> (IrDocument, Vec<Diagnostic>) {
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::with_limits(self.limits);
        self.evaluate_with_context(document, &mut diagnostics, &mut context)
    }

    /// Evaluates an IR document with access to an explicit semantic resource
    /// provider. The provider is retained only for this evaluation; the
    /// engine performs no filesystem or network I/O.
    pub fn evaluate_project<R: ResourceProvider>(
        &self,
        resources: &R,
        source_id: SourceId,
        document: &IrDocument,
    ) -> (IrDocument, Vec<Diagnostic>) {
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::with_resources(
            resources,
            source_id,
            &crate::DocumentMetadataDefaults::default(),
            self.limits,
        );
        self.evaluate_with_context(document, &mut diagnostics, &mut context)
    }

    /// Alias naming the engine-neutral input boundary explicitly.
    pub fn evaluate_with_resources<R: ResourceProvider>(
        &self,
        resources: &R,
        source_id: SourceId,
        document: &IrDocument,
        metadata_defaults: &crate::DocumentMetadataDefaults,
    ) -> (IrDocument, Vec<Diagnostic>) {
        let mut diagnostics = Vec::new();
        let mut context =
            EvaluationContext::with_resources(resources, source_id, metadata_defaults, self.limits);
        self.evaluate_with_context(document, &mut diagnostics, &mut context)
    }

    fn evaluate_with_context(
        &self,
        document: &IrDocument,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> (IrDocument, Vec<Diagnostic>) {
        context.initialize_document_state(&document.metadata.document_state);
        let nodes = self.evaluate_nodes(&document.nodes, diagnostics, context);
        (
            IrDocument {
                nodes,
                metadata: scribium_ir::IrMetadata {
                    document_state: context.document_state_snapshot(),
                    ..document.metadata.clone()
                },
            },
            std::mem::take(diagnostics),
        )
    }

    /// Evaluates a list of block nodes, collecting any diagnostics.
    fn evaluate_nodes(
        &self,
        nodes: &[IrNode],
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Vec<IrNode> {
        let mut out = Vec::new();
        for node in nodes {
            out.extend(self.evaluate_node(node, diagnostics, context));
        }
        out
    }

    /// Evaluates a single block node.
    fn evaluate_node(
        &self,
        node: &IrNode,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Vec<IrNode> {
        match node {
            IrNode::FunctionDeclaration {
                name,
                parameters,
                body,
                span,
            } => {
                self.handle_function_declaration(
                    name,
                    parameters,
                    body,
                    span,
                    diagnostics,
                    context,
                );
                Vec::new()
            }
            IrNode::FunctionCall {
                name,
                positional_args,
                named_args,
                lambda_parameters,
                body,
                span,
            } => match self.evaluate_block_call(
                name,
                positional_args,
                named_args,
                lambda_parameters.as_deref(),
                body.as_deref(),
                span,
                diagnostics,
                context,
            ) {
                CallOutcome::Value(IrValue::Content(nodes)) => nodes,
                CallOutcome::Value(_) | CallOutcome::NoValue | CallOutcome::Failed => Vec::new(),
                CallOutcome::Unresolved => Vec::new(),
            },
            IrNode::ChainedFunctionCall {
                head,
                chain,
                body,
                span,
            } => match self.evaluate_block_chain(head, chain, body, span, diagnostics, context) {
                CallOutcome::Value(IrValue::Content(nodes)) => nodes,
                CallOutcome::Value(_) | CallOutcome::NoValue | CallOutcome::Failed => Vec::new(),
                CallOutcome::Unresolved => Vec::new(),
            },
            IrNode::Heading {
                level,
                content,
                span,
            } => vec![IrNode::Heading {
                level: *level,
                content: self.evaluate_inlines(content, diagnostics, context),
                span: *span,
            }],
            IrNode::Paragraph { content, span } => vec![IrNode::Paragraph {
                content: self.evaluate_inlines(content, diagnostics, context),
                span: *span,
            }],
            IrNode::Blockquote { content, span } => vec![IrNode::Blockquote {
                content: self.evaluate_nodes(content, diagnostics, context),
                span: *span,
            }],
            IrNode::UnorderedList { items, span } => {
                let items = items
                    .iter()
                    .map(|item| scribium_ir::IrListItem {
                        nodes: self.evaluate_nodes(&item.nodes, diagnostics, context),
                        task: item.task,
                        span: item.span,
                    })
                    .collect();
                vec![IrNode::UnorderedList { items, span: *span }]
            }
            IrNode::OrderedList { items, start, span } => {
                let items = items
                    .iter()
                    .map(|item| scribium_ir::IrListItem {
                        nodes: self.evaluate_nodes(&item.nodes, diagnostics, context),
                        task: item.task,
                        span: item.span,
                    })
                    .collect();
                vec![IrNode::OrderedList {
                    items,
                    start: *start,
                    span: *span,
                }]
            }
            IrNode::Table { header, rows, span } => vec![IrNode::Table {
                header: scribium_ir::IrTableRow {
                    cells: header
                        .cells
                        .iter()
                        .map(|cell| scribium_ir::IrTableCell {
                            content: self.evaluate_inlines(&cell.content, diagnostics, context),
                            alignment: cell.alignment,
                            span: cell.span,
                        })
                        .collect(),
                    span: header.span,
                },
                rows: rows
                    .iter()
                    .map(|row| scribium_ir::IrTableRow {
                        cells: row
                            .cells
                            .iter()
                            .map(|cell| scribium_ir::IrTableCell {
                                content: self.evaluate_inlines(&cell.content, diagnostics, context),
                                alignment: cell.alignment,
                                span: cell.span,
                            })
                            .collect(),
                        span: row.span,
                    })
                    .collect(),
                span: *span,
            }],
            IrNode::RawHtml { span, .. } => {
                diagnostics.push(unsupported_raw_html(*span));
                Vec::new()
            }
            IrNode::TargetSpecificContent { content } => {
                vec![IrNode::TargetSpecificContent {
                    content: content.clone(),
                }]
            }
            other => vec![other.clone()],
        }
    }

    /// Evaluates inline content, collecting any diagnostics.
    fn evaluate_inlines(
        &self,
        inlines: &[IrInline],
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Vec<IrInline> {
        let mut out = Vec::new();
        for inline in inlines {
            out.extend(self.evaluate_inline(inline, diagnostics, context));
        }
        out
    }

    /// Evaluates a single inline node.
    fn evaluate_inline(
        &self,
        inline: &IrInline,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Vec<IrInline> {
        match inline {
            IrInline::Emphasis { content, span } => vec![IrInline::Emphasis {
                content: self.evaluate_inlines(content, diagnostics, context),
                span: *span,
            }],
            IrInline::Strong { content, span } => vec![IrInline::Strong {
                content: self.evaluate_inlines(content, diagnostics, context),
                span: *span,
            }],
            IrInline::Strikethrough { content, span } => vec![IrInline::Strikethrough {
                content: self.evaluate_inlines(content, diagnostics, context),
                span: *span,
            }],
            IrInline::Link {
                content,
                destination,
                title,
                span,
            } => vec![IrInline::Link {
                content: self.evaluate_inlines(content, diagnostics, context),
                destination: destination.clone(),
                title: title.clone(),
                span: *span,
            }],
            IrInline::DirectiveCall {
                name,
                positional_args,
                named_args,
                body,
                span,
            } => self.evaluate_inline_call(
                name,
                positional_args,
                named_args,
                body.as_deref(),
                span,
                diagnostics,
                context,
            ),
            IrInline::ChainedDirectiveCall {
                head,
                chain,
                body,
                span,
            } => self.evaluate_inline_chain(head, chain, body, span, diagnostics, context),
            IrInline::RawHtml { span, .. } => {
                diagnostics.push(unsupported_raw_html(*span));
                Vec::new()
            }
            IrInline::TargetSpecificContent { content } => {
                vec![IrInline::TargetSpecificContent {
                    content: content.clone(),
                }]
            }
            IrInline::Code { content, span } => {
                // Code spans are opaque: the content is never resolved,
                // recursed into, or evaluated. It passes straight through.
                vec![IrInline::Code {
                    content: content.clone(),
                    span: *span,
                }]
            }
            other => vec![other.clone()],
        }
    }

    /// Evaluates an ordinary block call in output context.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_block_call(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        lambda_parameters: Option<&[IrParameter]>,
        body: Option<&[IrNode]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        match self.evaluate_call_value(
            name,
            positional_args,
            named_args,
            body.map(CallBody::Block),
            lambda_parameters,
            span,
            diagnostics,
            context,
        ) {
            CallOutcome::Value(value) => {
                match self.materialize_block_value(value, span, diagnostics) {
                    Ok(nodes) => CallOutcome::Value(IrValue::Content(nodes)),
                    Err(outcome) => outcome,
                }
            }
            CallOutcome::NoValue => CallOutcome::NoValue,
            CallOutcome::Failed => CallOutcome::Failed,
            CallOutcome::Unresolved => match self.preserve_block_call(
                name,
                positional_args,
                named_args,
                lambda_parameters,
                body,
                span,
                diagnostics,
                context,
            ) {
                Ok(nodes) => CallOutcome::Value(IrValue::Content(nodes)),
                Err(outcome) => outcome,
            },
        }
    }

    /// Evaluates an ordinary inline call in output context.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_inline_call(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<&[IrInline]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Vec<IrInline> {
        if is_stacked_layout(name) {
            diagnostics.push(stacked_inline_materialization_error(*span));
            return Vec::new();
        }
        if is_center(name) && context.get_function(name).is_none() {
            diagnostics.push(center_inline_materialization_error(*span));
            return Vec::new();
        }
        if is_align(name) && context.get_function(name).is_none() {
            diagnostics.push(align_inline_materialization_error(*span));
            return Vec::new();
        }
        if is_container(name) && context.get_function(name).is_none() {
            diagnostics.push(container_inline_materialization_error(*span));
            return Vec::new();
        }
        if is_landscape(name) && context.get_function(name).is_none() {
            diagnostics.push(landscape_inline_materialization_error(*span));
            return Vec::new();
        }
        match self.evaluate_call_value(
            name,
            positional_args,
            named_args,
            body.map(CallBody::Inline),
            None,
            span,
            diagnostics,
            context,
        ) {
            CallOutcome::Value(value) => {
                self.materialize_inline_value(Some(value), span, diagnostics)
            }
            CallOutcome::NoValue => Vec::new(),
            CallOutcome::Failed => Vec::new(),
            CallOutcome::Unresolved => self
                .preserve_inline_call(
                    name,
                    positional_args,
                    named_args,
                    body,
                    span,
                    diagnostics,
                    context,
                )
                .unwrap_or_default(),
        }
    }

    /// Evaluates a block chain and materializes its final semantic value.
    fn evaluate_block_chain(
        &self,
        head: &IrCallSegment,
        chain: &[IrCallSegment],
        body: &Option<Vec<IrNode>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        match self.evaluate_chain_value(
            head,
            chain,
            body.as_deref().map(CallBody::Block),
            diagnostics,
            context,
        ) {
            CallOutcome::Value(value) => {
                match self.materialize_block_value(value, span, diagnostics) {
                    Ok(nodes) => CallOutcome::Value(IrValue::Content(nodes)),
                    Err(outcome) => outcome,
                }
            }
            CallOutcome::NoValue => CallOutcome::NoValue,
            CallOutcome::Failed => CallOutcome::Failed,
            CallOutcome::Unresolved => CallOutcome::Unresolved,
        }
    }

    /// Evaluates an inline chain and materializes its final semantic value.
    fn evaluate_inline_chain(
        &self,
        head: &IrCallSegment,
        chain: &[IrCallSegment],
        body: &Option<Vec<IrInline>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Vec<IrInline> {
        match self.evaluate_chain_value(
            head,
            chain,
            body.as_deref().map(CallBody::Inline),
            diagnostics,
            context,
        ) {
            CallOutcome::Value(value) => {
                self.materialize_inline_value(Some(value), span, diagnostics)
            }
            CallOutcome::NoValue | CallOutcome::Failed | CallOutcome::Unresolved => Vec::new(),
        }
    }

    /// Invokes a call in value context. Ordinary nested calls and chain
    /// segments use this exact contract; only their surrounding syntax differs.
    /// Bodies remain unevaluated until the callee selects an evaluation policy.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_call_value(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        self.evaluate_call_value_with_first_origin(
            name,
            positional_args,
            named_args,
            body,
            lambda_parameters,
            span,
            diagnostics,
            context,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_call_value_with_first_origin(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> CallOutcome {
        let _depth = match context.enter_evaluation_depth(*span, diagnostics) {
            Ok(depth) => depth,
            Err(outcome) => return outcome,
        };
        if let Some(result) = context.get_implicit_parameter(name) {
            return match result {
                Ok(value) => CallOutcome::Value(value),
                Err(error) => {
                    diagnostics.push(implicit_parameter_error(name, error, *span));
                    CallOutcome::Failed
                }
            };
        }

        if is_conditional(name) {
            let condition = match self.resolve_call_condition(
                name,
                positional_args,
                named_args,
                span,
                diagnostics,
                context,
                first_origin,
            ) {
                Ok(condition) => condition,
                Err(outcome) => return outcome,
            };
            return if take_branch(name, condition) {
                self.conditional_content_value(
                    positional_args,
                    named_args,
                    body,
                    span,
                    diagnostics,
                    context,
                )
            } else {
                CallOutcome::Value(IrValue::Content(Vec::new()))
            };
        }

        if is_document_state(name) {
            return self.evaluate_document_state_builtin(
                name,
                positional_args,
                named_args,
                body,
                span,
                diagnostics,
                context,
                first_origin,
            );
        }

        if is_html(name) {
            return self.evaluate_html(
                positional_args,
                named_args,
                body,
                span,
                diagnostics,
                context,
            );
        }

        if is_markdown(name) {
            return self.evaluate_markdown(
                positional_args,
                named_args,
                body,
                span,
                diagnostics,
                context,
            );
        }

        if is_resource(name) {
            return self.evaluate_resource_builtin(
                name,
                positional_args,
                named_args,
                body,
                span,
                diagnostics,
                context,
            );
        }

        if is_deferred(name) {
            diagnostics.push(resource_diagnostic(
                "E8001",
                "`.llmstxt` is not part of the tracked Quarkdown v2.5.1 standard builtin surface",
                *span,
                "This resource/document feature remains deferred until an evidenced upstream contract is available.",
            ));
            return CallOutcome::Failed;
        }

        if is_let(name) {
            return self.evaluate_let(
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
            );
        }

        if is_foreach(name) {
            return self.evaluate_foreach(
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
                first_origin,
            );
        }

        if is_repeat(name) {
            return self.evaluate_repeat(
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
            );
        }

        if is_optionality_callback(name) {
            return self.evaluate_optionality_callback(
                name,
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
            );
        }

        if is_collection_transform(name) {
            return self.evaluate_collection_transform(
                name,
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
                first_origin,
            );
        }

        if is_var_declaration(name) {
            return self.handle_var_declaration(
                positional_args,
                named_args,
                body,
                span,
                diagnostics,
                context,
            );
        }

        if is_variable_reference_call(name, positional_args, named_args, body, context) {
            return context
                .get(name)
                .map(|value| CallOutcome::Value(value.to_value()))
                .unwrap_or(CallOutcome::NoValue);
        }

        if is_variable_reassignment_call(name, positional_args, named_args, body, context) {
            return self.handle_variable_reassignment_value(
                name,
                positional_args,
                span,
                diagnostics,
                context,
            );
        }

        // A source-defined binding takes precedence over an evidenced native
        // builtin after its declaration has executed. The same value-context
        // dispatch is used by ordinary calls, nested arguments, and chains.
        if let Some(binding) = context.get_function(name).cloned() {
            return self.evaluate_user_function(
                &binding,
                positional_args,
                named_args,
                body,
                span,
                diagnostics,
                context,
            );
        }

        if is_center(name) {
            return self.evaluate_center(
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
            );
        }

        if is_align(name) {
            return self.evaluate_align(
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
                first_origin,
            );
        }

        if is_container(name) {
            return self.evaluate_container(
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
                first_origin,
            );
        }

        if is_landscape(name) {
            return self.evaluate_landscape(
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
            );
        }

        if is_br(name) {
            return self.evaluate_br(
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
            );
        }

        if is_whitespace(name) {
            return self.evaluate_whitespace(
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
                first_origin,
            );
        }

        if is_stacked_layout(name) {
            return self.evaluate_stacked_layout(
                name,
                positional_args,
                named_args,
                body,
                lambda_parameters,
                span,
                diagnostics,
                context,
                first_origin,
            );
        }

        if is_range(name) {
            return self.evaluate_range(
                positional_args,
                named_args,
                body,
                span,
                diagnostics,
                context,
                first_origin,
            );
        }

        if is_pair(name) {
            return self.evaluate_pair(
                positional_args,
                named_args,
                body,
                span,
                diagnostics,
                context,
            );
        }

        if is_dictionary(name) {
            return self.evaluate_dictionary(
                positional_args,
                named_args,
                body,
                span,
                diagnostics,
                context,
            );
        }

        if is_collection_access(name) {
            if body.is_some() {
                diagnostics.push(iteration_error(
                    format!("`.{name}` does not accept a block body"),
                    *span,
                ));
                return CallOutcome::Failed;
            }
            let evaluated_positional = match self.evaluate_invocation_values(
                positional_args,
                span,
                diagnostics,
                context,
                first_origin,
            ) {
                Ok(values) => values,
                Err(outcome) => return outcome,
            };
            let evaluated_named =
                match self.evaluate_invocation_named(named_args, span, diagnostics, context) {
                    Ok(values) => values,
                    Err(outcome) => return outcome,
                };
            return self.evaluate_collection_access(
                name,
                &evaluated_positional,
                &evaluated_named,
                span,
                diagnostics,
            );
        }

        if let Some(builtin) = builtins::lookup(name) {
            let evaluated_positional = match self.evaluate_invocation_values(
                positional_args,
                span,
                diagnostics,
                context,
                first_origin,
            ) {
                Ok(values) => values,
                Err(outcome) => return outcome,
            };
            let mut evaluated_positional = evaluated_positional;
            let has_body = match builtin.body_policy {
                builtins::BuiltinBodyPolicy::Reject => body.is_some(),
                builtins::BuiltinBodyPolicy::BindEvaluatedContent => {
                    if let Some(body) = body {
                        let body = match self.evaluate_call_body(body, span, diagnostics, context) {
                            CallOutcome::Value(value) => value,
                            outcome => return outcome,
                        };
                        evaluated_positional.push(InvocationValue::static_value(body));
                    }
                    false
                }
            };
            let evaluated_named =
                match self.evaluate_invocation_named(named_args, span, diagnostics, context) {
                    Ok(values) => values,
                    Err(outcome) => return outcome,
                };
            return match builtins::evaluate_with_origins(
                builtin,
                &evaluated_positional,
                &evaluated_named,
                has_body,
            ) {
                Ok(value) => CallOutcome::Value(value),
                Err(error) => {
                    diagnostics.push(chain_evaluation_error(error.message, *span));
                    CallOutcome::Failed
                }
            };
        }

        // Ordinary output context preserves unresolved calls. A chain wrapper
        // converts this outcome into an explicit source-backed E3001 instead.
        CallOutcome::Unresolved
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_center(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if !positional_args.is_empty() {
            diagnostics.push(center_argument_error(
                "`.center` does not accept positional arguments",
                value_source_span(&positional_args[0], span),
            ));
            return CallOutcome::Failed;
        }
        if let Some(argument) = named_args.first() {
            diagnostics.push(center_argument_error(
                "`.center` does not accept named arguments",
                argument.span,
            ));
            return CallOutcome::Failed;
        }
        if let Some(parameters) = lambda_parameters {
            let diagnostic_span = parameters.first().map_or(*span, |parameter| parameter.span);
            diagnostics.push(center_argument_error(
                "`.center` body is a Markdown block, not a lambda",
                diagnostic_span,
            ));
            return CallOutcome::Failed;
        }

        let children = match body {
            Some(CallBody::Block(nodes)) => {
                match self.evaluate_call_body(CallBody::Block(nodes), span, diagnostics, context) {
                    CallOutcome::Value(IrValue::Content(nodes)) => nodes,
                    outcome => return outcome,
                }
            }
            Some(CallBody::Inline(_)) => {
                diagnostics.push(center_argument_error("`.center` is block-only", *span));
                return CallOutcome::Failed;
            }
            None => {
                diagnostics.push(center_argument_error(
                    "`.center` requires a Markdown block body",
                    *span,
                ));
                return CallOutcome::Failed;
            }
        };

        CallOutcome::Value(IrValue::Component(IrComponent::Container(
            IrContainerComponent {
                width: None,
                height: None,
                full_width: true,
                alignment: Some(IrContainerAlignment::Center),
                children,
                span: *span,
            },
        )))
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_landscape(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if let Some(argument) = positional_args.first() {
            diagnostics.push(landscape_argument_error(
                "`.landscape` does not accept positional arguments",
                value_source_span(argument, span),
            ));
            return CallOutcome::Failed;
        }
        if let Some(argument) = named_args.first() {
            diagnostics.push(landscape_argument_error(
                "`.landscape` does not accept named arguments",
                argument.span,
            ));
            return CallOutcome::Failed;
        }
        if let Some(parameters) = lambda_parameters {
            let diagnostic_span = parameters.first().map_or(*span, |parameter| parameter.span);
            diagnostics.push(landscape_argument_error(
                "`.landscape` body is a Markdown block, not a lambda",
                diagnostic_span,
            ));
            return CallOutcome::Failed;
        }

        let children = match body {
            Some(CallBody::Block(nodes)) => {
                match self.evaluate_call_body(CallBody::Block(nodes), span, diagnostics, context) {
                    CallOutcome::Value(IrValue::Content(nodes)) => nodes,
                    outcome => return outcome,
                }
            }
            Some(CallBody::Inline(_)) => {
                diagnostics.push(landscape_argument_error(
                    "`.landscape` is block-only",
                    *span,
                ));
                return CallOutcome::Failed;
            }
            None => {
                diagnostics.push(landscape_argument_error(
                    "`.landscape` requires a Markdown block body",
                    *span,
                ));
                return CallOutcome::Failed;
            }
        };

        CallOutcome::Value(IrValue::Component(IrComponent::Landscape(
            IrLandscapeComponent {
                children,
                span: *span,
            },
        )))
    }

    fn evaluate_br(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> CallOutcome {
        if let Some(argument) = positional_args.first() {
            diagnostics.push(br_argument_error(
                "`.br` does not accept positional arguments",
                value_source_span(argument, span),
            ));
            return CallOutcome::Failed;
        }
        if let Some(argument) = named_args.first() {
            diagnostics.push(br_argument_error(
                "`.br` does not accept named arguments",
                argument.span,
            ));
            return CallOutcome::Failed;
        }
        if let Some(parameters) = lambda_parameters {
            let diagnostic_span = parameters.first().map_or(*span, |parameter| parameter.span);
            diagnostics.push(br_argument_error(
                "`.br` does not accept a lambda body",
                diagnostic_span,
            ));
            return CallOutcome::Failed;
        }
        if body.is_some() {
            diagnostics.push(br_argument_error("`.br` does not accept a body", *span));
            return CallOutcome::Failed;
        }

        CallOutcome::Value(IrValue::Content(vec![IrNode::Paragraph {
            content: vec![IrInline::HardBreak { span: *span }],
            span: *span,
        }]))
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_whitespace(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> CallOutcome {
        let positional_values = match self.evaluate_invocation_values(
            positional_args,
            span,
            diagnostics,
            context,
            first_origin,
        ) {
            Ok(values) => values,
            Err(outcome) => return outcome,
        };
        let named_values =
            match self.evaluate_invocation_named(named_args, span, diagnostics, context) {
                Ok(values) => values,
                Err(outcome) => return outcome,
            };
        let positional = positional_values
            .into_iter()
            .zip(positional_args.iter())
            .map(|(value, source)| WhitespaceArgument {
                value,
                span: value_source_span(source, span),
            })
            .collect();
        let bound = match bind_whitespace_arguments(positional, named_values, span, diagnostics) {
            Ok(bound) => bound,
            Err(outcome) => return outcome,
        };

        let width = match bound.width.as_ref() {
            Some(argument) => match convert_whitespace_size(&argument.value) {
                Ok(value) => value,
                Err(error) => {
                    diagnostics.push(whitespace_conversion_error("width", argument.span, error));
                    return CallOutcome::Failed;
                }
            },
            None => None,
        };
        let height = match bound.height.as_ref() {
            Some(argument) => match convert_whitespace_size(&argument.value) {
                Ok(value) => value,
                Err(error) => {
                    diagnostics.push(whitespace_conversion_error("height", argument.span, error));
                    return CallOutcome::Failed;
                }
            },
            None => None,
        };

        if let Some(parameters) = lambda_parameters {
            let diagnostic_span = parameters.first().map_or(*span, |parameter| parameter.span);
            diagnostics.push(whitespace_argument_error(
                "`.whitespace` does not accept a lambda body",
                diagnostic_span,
            ));
            return CallOutcome::Failed;
        }
        if body.is_some() {
            diagnostics.push(whitespace_argument_error(
                "`.whitespace` does not accept a body",
                *span,
            ));
            return CallOutcome::Failed;
        }

        let (width, height) = match (width, height) {
            (None, None) => (None, None),
            (width, height) => (
                Some(width.unwrap_or_else(zero_whitespace_size)),
                Some(height.unwrap_or_else(zero_whitespace_size)),
            ),
        };
        CallOutcome::Value(IrValue::Content(vec![IrNode::Paragraph {
            content: vec![IrInline::Whitespace {
                width,
                height,
                span: *span,
            }],
            span: *span,
        }]))
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_align(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> CallOutcome {
        let positional_values = match self.evaluate_invocation_values(
            positional_args,
            span,
            diagnostics,
            context,
            first_origin,
        ) {
            Ok(values) => values,
            Err(outcome) => return outcome,
        };
        let named_values =
            match self.evaluate_invocation_named(named_args, span, diagnostics, context) {
                Ok(values) => values,
                Err(outcome) => return outcome,
            };
        let positional = positional_values
            .into_iter()
            .zip(positional_args.iter())
            .map(|(value, source)| AlignArgument {
                value,
                span: value_source_span(source, span),
            })
            .collect();
        let alignment = match bind_align_argument(positional, named_values, span, diagnostics) {
            Ok(argument) => argument,
            Err(outcome) => return outcome,
        };
        let alignment = match convert_align_alignment(&alignment.value) {
            Ok(value) => value,
            Err(error) => {
                diagnostics.push(align_conversion_error(alignment.span, error));
                return CallOutcome::Failed;
            }
        };

        if let Some(parameters) = lambda_parameters {
            let diagnostic_span = parameters.first().map_or(*span, |parameter| parameter.span);
            diagnostics.push(align_argument_error(
                "`.align` body is a Markdown block, not a lambda",
                diagnostic_span,
            ));
            return CallOutcome::Failed;
        }
        let children = match body {
            Some(CallBody::Block(nodes)) => {
                match self.evaluate_call_body(CallBody::Block(nodes), span, diagnostics, context) {
                    CallOutcome::Value(IrValue::Content(nodes)) => nodes,
                    outcome => return outcome,
                }
            }
            Some(CallBody::Inline(_)) => {
                diagnostics.push(align_argument_error("`.align` is block-only", *span));
                return CallOutcome::Failed;
            }
            None => {
                diagnostics.push(align_argument_error(
                    "`.align` requires a Markdown block body",
                    *span,
                ));
                return CallOutcome::Failed;
            }
        };

        CallOutcome::Value(IrValue::Component(IrComponent::Container(
            IrContainerComponent {
                width: None,
                height: None,
                full_width: true,
                alignment: Some(alignment),
                children,
                span: *span,
            },
        )))
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_container(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> CallOutcome {
        let positional_values = match self.evaluate_invocation_values(
            positional_args,
            span,
            diagnostics,
            context,
            first_origin,
        ) {
            Ok(values) => values,
            Err(outcome) => return outcome,
        };
        let named_values =
            match self.evaluate_invocation_named(named_args, span, diagnostics, context) {
                Ok(values) => values,
                Err(outcome) => return outcome,
            };
        let positional = positional_values
            .into_iter()
            .zip(positional_args.iter())
            .map(|(value, source)| ContainerArgument {
                value,
                span: value_source_span(source, span),
            })
            .collect();
        let bound = match bind_container_arguments(positional, named_values, span, diagnostics) {
            Ok(bound) => bound,
            Err(outcome) => return outcome,
        };

        let width = match bound.width.as_ref() {
            Some(argument) if matches!(&argument.value.value, IrValue::None) => None,
            Some(argument) => match convert_container_size(&argument.value) {
                Ok(value) => Some(value),
                Err(error) => {
                    diagnostics.push(container_conversion_error("width", argument.span, error));
                    return CallOutcome::Failed;
                }
            },
            None => None,
        };
        let height = match bound.height.as_ref() {
            Some(argument) if matches!(&argument.value.value, IrValue::None) => None,
            Some(argument) => match convert_container_size(&argument.value) {
                Ok(value) => Some(value),
                Err(error) => {
                    diagnostics.push(container_conversion_error("height", argument.span, error));
                    return CallOutcome::Failed;
                }
            },
            None => None,
        };
        let full_width = match bound.full_width.as_ref() {
            Some(argument) => match convert_container_boolean(&argument.value) {
                Ok(value) => value,
                Err(error) => {
                    diagnostics.push(container_conversion_error(
                        "fullwidth",
                        argument.span,
                        error,
                    ));
                    return CallOutcome::Failed;
                }
            },
            None => false,
        };

        if let Some(parameters) = lambda_parameters {
            let diagnostic_span = parameters.first().map_or(*span, |parameter| parameter.span);
            diagnostics.push(container_argument_error_at(
                "`.container` body is a Markdown block, not a lambda".to_string(),
                diagnostic_span,
            ));
            return CallOutcome::Failed;
        }

        let children = match body {
            Some(CallBody::Block(nodes)) => {
                match self.evaluate_call_body(CallBody::Block(nodes), span, diagnostics, context) {
                    CallOutcome::Value(IrValue::Content(nodes)) => nodes,
                    outcome => return outcome,
                }
            }
            Some(CallBody::Inline(_)) => {
                diagnostics.push(container_argument_error(
                    "`.container` is block-only",
                    *span,
                ));
                return CallOutcome::Failed;
            }
            None => Vec::new(),
        };

        CallOutcome::Value(IrValue::Component(IrComponent::Container(
            IrContainerComponent {
                width,
                height,
                full_width,
                alignment: None,
                children,
                span: *span,
            },
        )))
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_stacked_layout(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> CallOutcome {
        let positional_values = match self.evaluate_invocation_values(
            positional_args,
            span,
            diagnostics,
            context,
            first_origin,
        ) {
            Ok(values) => values,
            Err(outcome) => return outcome,
        };
        let named_values =
            match self.evaluate_invocation_named(named_args, span, diagnostics, context) {
                Ok(values) => values,
                Err(outcome) => return outcome,
            };
        let positional = positional_values
            .into_iter()
            .zip(positional_args.iter())
            .map(|(value, source)| StackedArgument {
                value,
                span: value_source_span(source, span),
            })
            .collect();
        let bound = match bind_stacked_arguments(name, positional, named_values, span, diagnostics)
        {
            Ok(bound) => bound,
            Err(outcome) => return outcome,
        };
        let mut bound = bound;
        let default = |value| StackedArgument {
            value: InvocationValue::static_value(value),
            span: *span,
        };

        let (layout, main_axis, cross_axis, row_gap, column_gap) = match name {
            "row" | "column" => {
                let alignment = bound.take(0).unwrap_or_else(|| {
                    default(IrValue::Enum(IrEnumValue::StackedMainAxisAlignment(
                        IrMainAxisAlignment::Start,
                    )))
                });
                let cross = bound.take(1).unwrap_or_else(|| {
                    default(IrValue::Enum(IrEnumValue::StackedCrossAxisAlignment(
                        IrCrossAxisAlignment::Center,
                    )))
                });
                let gap = bound.take(2).unwrap_or_else(|| default(IrValue::None));
                let main_axis = match convert_stacked_main_axis(&alignment.value) {
                    Ok(value) => value,
                    Err(error) => {
                        diagnostics.push(stacked_conversion_error(
                            name,
                            "alignment",
                            alignment.span,
                            error,
                        ));
                        return CallOutcome::Failed;
                    }
                };
                let cross_axis = match convert_stacked_cross_axis(&cross.value) {
                    Ok(value) => value,
                    Err(error) => {
                        diagnostics
                            .push(stacked_conversion_error(name, "cross", cross.span, error));
                        return CallOutcome::Failed;
                    }
                };
                let gap = match convert_optional_stacked_size(&gap.value) {
                    Ok(value) => value,
                    Err(error) => {
                        diagnostics.push(stacked_conversion_error(name, "gap", gap.span, error));
                        return CallOutcome::Failed;
                    }
                };
                let (layout, row_gap, column_gap) = if name == "row" {
                    (IrStackedLayout::Row, None, gap)
                } else {
                    (IrStackedLayout::Column, gap, None)
                };
                (layout, main_axis, cross_axis, row_gap, column_gap)
            }
            "grid" => {
                let Some(columns) = bound.take(0) else {
                    diagnostics.push(stacked_argument_error(
                        name,
                        "columns",
                        *span,
                        "required argument is missing",
                    ));
                    return CallOutcome::Failed;
                };
                let alignment = bound.take(1).unwrap_or_else(|| {
                    default(IrValue::Enum(IrEnumValue::StackedMainAxisAlignment(
                        IrMainAxisAlignment::Center,
                    )))
                });
                let cross = bound.take(2).unwrap_or_else(|| {
                    default(IrValue::Enum(IrEnumValue::StackedCrossAxisAlignment(
                        IrCrossAxisAlignment::Center,
                    )))
                });
                let gap = bound.take(3).unwrap_or_else(|| default(IrValue::None));
                let vgap = bound.take(4).unwrap_or_else(|| default(IrValue::None));
                let hgap = bound.take(5).unwrap_or_else(|| default(IrValue::None));
                let columns = match value_conversion::convert_integer_with_origin(&columns.value) {
                    Ok(value) => value,
                    Err(error) => {
                        diagnostics.push(stacked_conversion_error(
                            name,
                            "columns",
                            columns.span,
                            error,
                        ));
                        return CallOutcome::Failed;
                    }
                };
                if columns <= 0 {
                    diagnostics.push(stacked_argument_error(
                        name,
                        "columns",
                        *span,
                        "Column count must be at least 1",
                    ));
                    return CallOutcome::Failed;
                }
                let Some(columns) = NonZeroU32::new(columns as u32) else {
                    diagnostics.push(stacked_argument_error(
                        name,
                        "columns",
                        *span,
                        "Column count must be at least 1",
                    ));
                    return CallOutcome::Failed;
                };
                let main_axis = match convert_stacked_main_axis(&alignment.value) {
                    Ok(value) => value,
                    Err(error) => {
                        diagnostics.push(stacked_conversion_error(
                            name,
                            "alignment",
                            alignment.span,
                            error,
                        ));
                        return CallOutcome::Failed;
                    }
                };
                let cross_axis = match convert_stacked_cross_axis(&cross.value) {
                    Ok(value) => value,
                    Err(error) => {
                        diagnostics
                            .push(stacked_conversion_error(name, "cross", cross.span, error));
                        return CallOutcome::Failed;
                    }
                };
                let gap = match convert_optional_stacked_size(&gap.value) {
                    Ok(value) => value,
                    Err(error) => {
                        diagnostics.push(stacked_conversion_error(name, "gap", gap.span, error));
                        return CallOutcome::Failed;
                    }
                };
                let vgap = match convert_optional_stacked_size(&vgap.value) {
                    Ok(value) => value,
                    Err(error) => {
                        diagnostics.push(stacked_conversion_error(name, "vgap", vgap.span, error));
                        return CallOutcome::Failed;
                    }
                };
                let hgap = match convert_optional_stacked_size(&hgap.value) {
                    Ok(value) => value,
                    Err(error) => {
                        diagnostics.push(stacked_conversion_error(name, "hgap", hgap.span, error));
                        return CallOutcome::Failed;
                    }
                };
                (
                    IrStackedLayout::Grid { columns },
                    main_axis,
                    cross_axis,
                    vgap.or_else(|| gap.clone()),
                    hgap.or(gap),
                )
            }
            _ => return CallOutcome::Unresolved,
        };

        if let Some(parameters) = lambda_parameters {
            let diagnostic_span = parameters.first().map_or(*span, |parameter| parameter.span);
            diagnostics.push(stacked_argument_error(
                name,
                "body",
                diagnostic_span,
                "Stacked layout bodies are Markdown blocks, not lambda parameters",
            ));
            return CallOutcome::Failed;
        }
        let children = match body {
            Some(CallBody::Block(nodes)) => {
                match self.evaluate_call_body(CallBody::Block(nodes), span, diagnostics, context) {
                    CallOutcome::Value(IrValue::Content(nodes)) => nodes,
                    outcome => return outcome,
                }
            }
            Some(CallBody::Inline(_)) => {
                diagnostics.push(stacked_argument_error(
                    name,
                    "body",
                    *span,
                    "Stacked layout is block-only",
                ));
                return CallOutcome::Failed;
            }
            None => {
                diagnostics.push(stacked_argument_error(
                    name,
                    "body",
                    *span,
                    "A Markdown block body is required",
                ));
                return CallOutcome::Failed;
            }
        };

        CallOutcome::Value(IrValue::Component(IrComponent::Stacked(
            IrStackedComponent {
                layout,
                main_axis_alignment: main_axis,
                cross_axis_alignment: cross_axis,
                row_gap,
                column_gap,
                children,
                span: *span,
            },
        )))
    }

    /// Implements the read/write document-state builtins without changing
    /// the ordinary lexical scope maps. Argument evaluation and bounded
    /// String conversion complete before the shared state is mutated.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_document_state_builtin(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> CallOutcome {
        if body.is_some() {
            diagnostics.push(document_state_call_error(
                format!("`.{name}` does not accept a block body"),
                *span,
            ));
            return CallOutcome::Failed;
        }

        if positional_args.is_empty() && named_args.is_empty() {
            return CallOutcome::Value(context.document_state_value(name));
        }

        let evaluated_positional = match self.evaluate_invocation_values(
            positional_args,
            span,
            diagnostics,
            context,
            first_origin,
        ) {
            Ok(values) => values,
            Err(outcome) => return outcome,
        };
        let evaluated_named =
            match self.evaluate_invocation_named(named_args, span, diagnostics, context) {
                Ok(values) => values,
                Err(outcome) => return outcome,
            };

        if name == "doctype" {
            let argument_span = if evaluated_positional.len() == 1 && evaluated_named.is_empty() {
                positional_args
                    .first()
                    .map(|value| value_source_span(value, span))
                    .unwrap_or(*span)
            } else if evaluated_positional.is_empty()
                && evaluated_named.len() == 1
                && evaluated_named[0].name == "type"
            {
                named_args
                    .first()
                    .map(|argument| argument.span)
                    .unwrap_or(*span)
            } else {
                diagnostics.push(document_state_call_error(
                    "`.doctype` accepts exactly one positional or `type` argument".to_string(),
                    *span,
                ));
                return CallOutcome::Failed;
            };

            let argument = if evaluated_positional.len() == 1 && evaluated_named.is_empty() {
                let mut values = evaluated_positional.into_iter();
                let Some(argument) = values.next() else {
                    diagnostics.push(document_state_call_error(
                        "`.doctype` requires a document type argument".to_string(),
                        *span,
                    ));
                    return CallOutcome::Failed;
                };
                argument
            } else {
                let Some(argument) = evaluated_named.first() else {
                    diagnostics.push(document_state_call_error(
                        "`.doctype` requires a document type argument".to_string(),
                        *span,
                    ));
                    return CallOutcome::Failed;
                };
                InvocationValue {
                    value: argument.value.clone(),
                    origin: argument.origin,
                }
            };

            let document_type = match value_conversion::convert_domain_with_origin(
                &argument,
                value_conversion::DomainTarget::ClosedEnum(
                    value_conversion::ClosedEnumTarget::DocumentType,
                ),
            ) {
                Ok(value_conversion::DomainValue::Enum(
                    scribium_ir::IrEnumValue::DocumentType(value),
                )) => value,
                Ok(_) | Err(_) => {
                    diagnostics.push(document_state_conversion_error(
                        "`.doctype` requires one of `plain`, `paged`, `slides`, or `docs` from a dynamic argument"
                            .to_string(),
                        argument_span,
                    ));
                    return CallOutcome::Failed;
                }
            };
            context.set_document_type(document_type);
            return CallOutcome::NoValue;
        }

        if evaluated_positional.len() != 1 || !evaluated_named.is_empty() {
            diagnostics.push(document_state_call_error(
                format!("`.{name}` accepts exactly one positional String argument when writing"),
                *span,
            ));
            return CallOutcome::Failed;
        }

        let argument = &evaluated_positional[0];
        let argument_span = positional_args
            .first()
            .map(|value| value_source_span(value, span))
            .unwrap_or(*span);
        let Some(value) = builtins::scalar_string_argument(argument) else {
            diagnostics.push(document_state_conversion_error(
                format!("`.{name}` requires a value that converts to String"),
                argument_span,
            ));
            return CallOutcome::Failed;
        };

        if name == "docname" && value.trim().is_empty() {
            diagnostics.push(document_state_conversion_error(
                "`.docname` cannot be blank".to_string(),
                argument_span,
            ));
            return CallOutcome::Failed;
        }

        context.set_document_state_value(name, value);
        CallOutcome::NoValue
    }

    /// Evaluates the closed Quarkdown `.html(content: String)` builtin.
    ///
    /// The result is kept in an ordinary content value so the existing block
    /// and inline materialization paths preserve placement independently.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_html(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if !self.capabilities.allows(Capability::NativeContent) {
            diagnostics.push(native_content_denied(*span));
            return CallOutcome::Failed;
        }

        if positional_args.len() > 1 {
            diagnostics.push(html_argument_error(
                "`.html` accepts exactly one `content` argument",
                *span,
            ));
            return CallOutcome::Failed;
        }

        let mut named_content = None;
        for argument in named_args {
            if argument.name != "content" {
                diagnostics.push(html_argument_error_at(
                    format!(
                        "`.html` does not support named argument `{}`",
                        argument.name
                    ),
                    argument.name_span,
                ));
                return CallOutcome::Failed;
            }
            if named_content.is_some() {
                diagnostics.push(html_argument_error_at(
                    "`.html` received named argument `content` more than once".to_string(),
                    argument.name_span,
                ));
                return CallOutcome::Failed;
            }
            named_content = Some(&argument.value);
        }

        if positional_args.len() == 1 && named_content.is_some() {
            diagnostics.push(html_argument_error(
                "`.html` received `content` more than once",
                *span,
            ));
            return CallOutcome::Failed;
        }
        if body.is_some() && (positional_args.len() == 1 || named_content.is_some()) {
            diagnostics.push(html_argument_error(
                "`.html` received both a body and an explicit `content` argument",
                *span,
            ));
            return CallOutcome::Failed;
        }

        let content = if let Some(body) = body {
            match self.evaluate_html_body(body, span, diagnostics, context) {
                CallOutcome::Value(value) => value,
                outcome => return outcome,
            }
        } else if let Some(value) = positional_args.first().or(named_content) {
            match self.evaluate_value(value, diagnostics, context) {
                CallOutcome::Value(value) => value,
                CallOutcome::Unresolved => {
                    match self.preserve_value_expression(value, diagnostics, context) {
                        Ok(value) => value,
                        Err(outcome) => return outcome,
                    }
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(value, span)));
                    return CallOutcome::Failed;
                }
                CallOutcome::Failed => return CallOutcome::Failed,
            }
        } else {
            diagnostics.push(html_argument_error(
                "`.html` requires one `content` argument or body",
                *span,
            ));
            return CallOutcome::Failed;
        };

        let Some(content) = builtins::adapt_string_argument(&content) else {
            diagnostics.push(html_argument_error(
                "`.html` content must adapt to the supported String boundary",
                *span,
            ));
            return CallOutcome::Failed;
        };

        CallOutcome::Value(IrValue::Content(vec![IrNode::TargetSpecificContent {
            content: TargetSpecificContent {
                target: NativeTarget::Html,
                content,
                span: *span,
            },
        }]))
    }

    fn evaluate_html_body(
        &self,
        body: CallBody<'_>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        match body {
            CallBody::Block(nodes) if body_contains_raw_html(nodes) => {
                match opaque_html_body_string(nodes) {
                    Some(content) => CallOutcome::Value(IrValue::String(content)),
                    None => {
                        diagnostics.push(html_argument_error(
                            "`.html` body contains structure that cannot adapt to String",
                            *span,
                        ));
                        CallOutcome::Failed
                    }
                }
            }
            body => self.evaluate_call_body(body, span, diagnostics, context),
        }
    }

    /// Evaluates Quarkdown's raw native Markdown-content builtin. This is
    /// intentionally not a file loader: the v2.5.1 contract accepts Markdown
    /// content and returns an opaque native-content node.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_markdown(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if !self.capabilities.allows(Capability::NativeContent) {
            diagnostics.push(Diagnostic {
                code: "E3004".to_string(),
                severity: Severity::Error,
                message: "NativeContent capability is required for `.markdown`".to_string(),
                primary: Some(*span),
                secondary: Vec::new(),
                hints: vec![
                    "Grant the NativeContent capability for this compilation to enable `.markdown`."
                        .to_string(),
                ],
            });
            return CallOutcome::Failed;
        }
        if positional_args.len() > 1 {
            diagnostics.push(resource_diagnostic(
                "E3003",
                "`.markdown` accepts exactly one `content` argument".to_string(),
                *span,
                "Pass Markdown content as the body or as `content`.",
            ));
            return CallOutcome::Failed;
        }
        let mut content_argument = None;
        for argument in named_args {
            if argument.name != "content" || content_argument.is_some() {
                diagnostics.push(resource_diagnostic(
                    "E3003",
                    format!(
                        "`.markdown` does not accept named argument `{}` more than once",
                        argument.name
                    ),
                    argument.name_span,
                    "Use one positional or `content` argument.",
                ));
                return CallOutcome::Failed;
            }
            content_argument = Some(&argument.value);
        }
        if positional_args.len() == 1 && content_argument.is_some() {
            diagnostics.push(resource_diagnostic(
                "E3003",
                "`.markdown` received `content` more than once".to_string(),
                *span,
                "Use either the positional argument or the named argument.",
            ));
            return CallOutcome::Failed;
        }
        if body.is_some() && (positional_args.len() == 1 || content_argument.is_some()) {
            diagnostics.push(resource_diagnostic(
                "E3003",
                "`.markdown` received both a body and an explicit `content` argument".to_string(),
                *span,
                "Use either the body or the explicit content argument.",
            ));
            return CallOutcome::Failed;
        }
        let content = if let Some(body) = body {
            match self.evaluate_call_body(body, span, diagnostics, context) {
                CallOutcome::Value(value) => value,
                outcome => return outcome,
            }
        } else if let Some(value) = positional_args.first().or(content_argument) {
            match self.evaluate_value(value, diagnostics, context) {
                CallOutcome::Value(value) => value,
                CallOutcome::Unresolved => {
                    match self.preserve_value_expression(value, diagnostics, context) {
                        Ok(value) => value,
                        Err(outcome) => return outcome,
                    }
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(value, span)));
                    return CallOutcome::Failed;
                }
                CallOutcome::Failed => return CallOutcome::Failed,
            }
        } else {
            diagnostics.push(resource_diagnostic(
                "E3003",
                "`.markdown` requires Markdown content".to_string(),
                *span,
                "Pass Markdown content as the body or as `content`.",
            ));
            return CallOutcome::Failed;
        };
        let Some(content) = builtins::adapt_string_argument(&content) else {
            diagnostics.push(resource_diagnostic(
                "E3003",
                "`.markdown` content must adapt to the supported String boundary".to_string(),
                *span,
                "Rich semantic values are not silently rendered into native Markdown text.",
            ));
            return CallOutcome::Failed;
        };
        CallOutcome::Value(IrValue::Content(vec![IrNode::TargetSpecificContent {
            content: TargetSpecificContent {
                target: NativeTarget::Markdown,
                content,
                span: *span,
            },
        }]))
    }

    /// Evaluates the resource-backed subset of the Quarkdown standard library.
    ///
    /// Resource access is deliberately routed through the host-supplied
    /// semantic provider. The evaluator never receives a native path and
    /// never performs filesystem or network I/O itself.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_resource_builtin(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if body.is_some() {
            diagnostics.push(resource_diagnostic(
                "E3003",
                format!("`.{name}` does not accept a block body"),
                *span,
                "Pass the logical project resource path as an argument.",
            ));
            return CallOutcome::Failed;
        }

        let evaluated_positional =
            match self.evaluate_values(positional_args, span, diagnostics, context) {
                Ok(values) => values,
                Err(outcome) => return outcome,
            };
        let evaluated_named = match self.evaluate_named(named_args, span, diagnostics, context) {
            Ok(values) => values,
            Err(outcome) => return outcome,
        };

        match name {
            "read" => self.evaluate_read(
                &evaluated_positional,
                &evaluated_named,
                span,
                diagnostics,
                context,
            ),
            "json" => self.evaluate_json(
                &evaluated_positional,
                &evaluated_named,
                span,
                diagnostics,
                context,
            ),
            "include" => self.evaluate_include(
                &evaluated_positional,
                &evaluated_named,
                span,
                diagnostics,
                context,
            ),
            _ => CallOutcome::Failed,
        }
    }

    fn evaluate_read(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &EvaluationContext<'_>,
    ) -> CallOutcome {
        let Some(reference) =
            resource_path_argument("read", positional_args, named_args, span, diagnostics)
        else {
            return CallOutcome::Failed;
        };
        let lines = match resource_lines_argument(named_args, span, diagnostics) {
            Ok(lines) => lines,
            Err(()) => return CallOutcome::Failed,
        };
        let Some((provider, source_id)) = resource_context(context, span, diagnostics) else {
            return CallOutcome::Failed;
        };
        let ResourceText { path, text } = match provider.read_text(source_id, &reference) {
            Ok(value) => value,
            Err(error) => {
                diagnostics.push(resource_access_diagnostic("read", error, *span));
                return CallOutcome::Failed;
            }
        };
        let value = match lines {
            None => normalize_line_separators(&text),
            Some(range) => match select_lines(&text, range) {
                Ok(value) => value,
                Err(message) => {
                    diagnostics.push(resource_diagnostic(
                        "E3001",
                        format!("`.read` cannot select lines from `{path}`: {message}"),
                        *span,
                        "Use a one-based, inclusive line range within the resource.",
                    ));
                    return CallOutcome::Failed;
                }
            },
        };
        CallOutcome::Value(IrValue::String(value))
    }

    fn evaluate_json(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &EvaluationContext<'_>,
    ) -> CallOutcome {
        let Some(reference) =
            resource_path_argument("json", positional_args, named_args, span, diagnostics)
        else {
            return CallOutcome::Failed;
        };
        if !named_args.is_empty() {
            diagnostics.push(resource_diagnostic(
                "E3003",
                "`.json` does not support named arguments".to_string(),
                named_args[0].name_span,
                "Pass exactly one logical project resource path.",
            ));
            return CallOutcome::Failed;
        }
        let Some((provider, source_id)) = resource_context(context, span, diagnostics) else {
            return CallOutcome::Failed;
        };
        let ResourceText { path, text } = match provider.read_text(source_id, &reference) {
            Ok(value) => value,
            Err(error) => {
                diagnostics.push(resource_access_diagnostic("json", error, *span));
                return CallOutcome::Failed;
            }
        };
        let value = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => value,
            Err(error) => {
                diagnostics.push(resource_diagnostic(
                    "E3001",
                    format!("`.json` could not parse `{path}`: {error}"),
                    *span,
                    "Provide valid UTF-8 JSON in the logical project resource.",
                ));
                return CallOutcome::Failed;
            }
        };
        match json_value_to_ir(&value, *span) {
            Ok(value) => CallOutcome::Value(value),
            Err(message) => {
                diagnostics.push(resource_diagnostic(
                    "E3001",
                    format!("`.json` value in `{path}` is unsupported: {message}"),
                    *span,
                    "Use JSON values representable by Scribium's typed evaluator model.",
                ));
                CallOutcome::Failed
            }
        }
    }

    fn evaluate_include(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        let Some(reference) =
            resource_path_argument("include", positional_args, named_args, span, diagnostics)
        else {
            return CallOutcome::Failed;
        };
        let sandbox = match include_sandbox_argument(named_args, span, diagnostics) {
            Ok(sandbox) => sandbox,
            Err(()) => return CallOutcome::Failed,
        };
        let Some((provider, source_id)) = resource_context(context, span, diagnostics) else {
            return CallOutcome::Failed;
        };
        let IncludedSource {
            path,
            source_id: target_id,
            text: source,
        } = match provider.read_source(source_id, &reference) {
            Ok(source) => source,
            Err(ResourceAccessError::NotFound { path }) => {
                diagnostics.push(resource_diagnostic(
                    "E3001",
                    format!("`.include` resource not found: `{path}`"),
                    *span,
                    "Add the target source to the VirtualProject supplied by the host.",
                ));
                return CallOutcome::Failed;
            }
            Err(error) => {
                diagnostics.push(resource_access_diagnostic("include", error, *span));
                return CallOutcome::Failed;
            }
        };
        if let Some(position) = context
            .active_sources
            .iter()
            .position(|id| *id == target_id)
        {
            let mut chain = context.active_sources[position..]
                .iter()
                .filter_map(|id| provider.source_path(*id))
                .collect::<Vec<_>>();
            chain.push(path.to_string());
            diagnostics.push(resource_diagnostic(
                "E3001",
                format!("`.include` cycle detected: {}", chain.join(" -> ")),
                *span,
                "An active include may not include a source already on its call stack.",
            ));
            return CallOutcome::Failed;
        }

        let mode = source_mode_for_resource_path(&path);
        let include_diagnostics_start = diagnostics.len();
        let parsed = scribium_markdown::parse_with_mode(&source, mode);
        for diagnostic in parsed.diagnostics {
            diagnostics.push(Diagnostic {
                code: diagnostic.code.to_string(),
                severity: Severity::Error,
                message: diagnostic.message,
                primary: Some(SourceSpan {
                    source_id: target_id,
                    start: diagnostic.span.start,
                    end: diagnostic.span.end,
                }),
                secondary: Vec::new(),
                hints: Vec::new(),
            });
        }
        if diagnostics.len() != include_diagnostics_start {
            return CallOutcome::Failed;
        }
        let (document, lowering_diagnostics) = ast_to_ir::ast_to_ir_with_diagnostics_for_mode(
            &parsed.document,
            target_id,
            &context.metadata_defaults,
            mode,
        );
        diagnostics.extend(lowering_diagnostics);
        if diagnostics.len() != include_diagnostics_start {
            return CallOutcome::Failed;
        }

        let previous_source = context.current_source;
        let previous_active = context.active_sources.clone();
        context.active_sources.push(target_id);
        let evaluation_diagnostics_start = diagnostics.len();
        let result = match sandbox {
            IncludeSandbox::Share => {
                context.current_source = Some(target_id);
                let result = self.evaluate_nodes(&document.nodes, diagnostics, context);
                context.current_source = previous_source;
                result
            }
            IncludeSandbox::Scope | IncludeSandbox::Subdocument => {
                let mut child = context.child();
                child.current_source = Some(target_id);
                child.active_sources = context.active_sources.clone();
                self.evaluate_nodes(&document.nodes, diagnostics, &mut child)
            }
        };
        context.active_sources = previous_active;
        if diagnostics.len() != evaluation_diagnostics_start {
            return CallOutcome::Failed;
        }
        CallOutcome::Value(IrValue::Content(result))
    }

    /// Evaluates the bounded Collection access operations through the same
    /// ordered semantic element adaptation used by `.foreach`.
    fn evaluate_collection_access(
        &self,
        name: &str,
        positional_args: &[InvocationValue],
        named_args: &[InvocationNamedArg],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> CallOutcome {
        match name {
            "size" | "first" | "second" | "third" | "last" | "sumall" | "average" | "distinct"
            | "reversed" | "groupvalues" => {
                let named_parameter = if name == "size" { "of" } else { "from" };
                let value = match collection_access_operand(
                    name,
                    named_parameter,
                    positional_args,
                    named_args,
                    span,
                    diagnostics,
                ) {
                    Ok(value) => value,
                    Err(outcome) => return outcome,
                };
                let elements = match self.coerce_iterable(value, span, diagnostics) {
                    Ok(elements) => elements,
                    Err(outcome) => return outcome,
                };
                match name {
                    "size" => match exact_collection_length(elements.len(), span, diagnostics) {
                        Ok(length) => CallOutcome::Value(IrValue::Number(length)),
                        Err(outcome) => outcome,
                    },
                    "first" => {
                        CallOutcome::Value(elements.first().cloned().unwrap_or(IrValue::None))
                    }
                    "second" => {
                        CallOutcome::Value(elements.get(1).cloned().unwrap_or(IrValue::None))
                    }
                    "third" => {
                        CallOutcome::Value(elements.get(2).cloned().unwrap_or(IrValue::None))
                    }
                    "last" => CallOutcome::Value(elements.last().cloned().unwrap_or(IrValue::None)),
                    "sumall" => CallOutcome::Value(IrValue::Number(collection_sum_all(&elements))),
                    "average" => match collection_average(&elements, span, diagnostics) {
                        Ok(average) => CallOutcome::Value(IrValue::Number(average)),
                        Err(outcome) => outcome,
                    },
                    "distinct" => distinct_collection_values(elements, *span, diagnostics),
                    "reversed" => {
                        let mut reversed = elements;
                        reversed.reverse();
                        CallOutcome::Value(IrValue::Collection(reversed))
                    }
                    "groupvalues" => group_collection_values(elements, *span, diagnostics),
                    _ => unreachable!("collection access operation was prevalidated"),
                }
            }
            "getat" => {
                let (value, index, fallback) =
                    match getat_operands(positional_args, named_args, span, diagnostics) {
                        Ok(operands) => operands,
                        Err(outcome) => return outcome,
                    };
                let elements = match self.coerce_iterable(value, span, diagnostics) {
                    Ok(elements) => elements,
                    Err(outcome) => return outcome,
                };
                let length = match exact_collection_length(elements.len(), span, diagnostics) {
                    Ok(length) => length,
                    Err(outcome) => return outcome,
                };
                let index = match collection_index(&index, length, span, diagnostics) {
                    Ok(index) => index,
                    Err(outcome) => return outcome,
                };
                CallOutcome::Value(
                    index
                        .and_then(|index| elements.get(index).cloned())
                        .unwrap_or(fallback),
                )
            }
            _ => unreachable!("collection access operation was prevalidated"),
        }
    }

    /// Evaluates `.pair` as a typed, recursively valued pair.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_pair(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if positional_args.len() != 2 {
            diagnostics.push(iteration_error(
                format!(
                    "`.pair` requires exactly two positional values (received {})",
                    positional_args.len()
                ),
                *span,
            ));
            return CallOutcome::Failed;
        }
        if let Some(argument) = named_args.first() {
            diagnostics.push(iteration_error_at(
                format!("Unknown named argument `{}` for `.pair`", argument.name),
                argument.name_span,
            ));
            return CallOutcome::Failed;
        }
        if body.is_some() {
            diagnostics.push(iteration_error(
                "`.pair` does not accept a block body".to_string(),
                *span,
            ));
            return CallOutcome::Failed;
        }
        let values = match self.evaluate_values(positional_args, span, diagnostics, context) {
            Ok(values) => values,
            Err(outcome) => return outcome,
        };
        let mut values = values.into_iter();
        let Some(first) = values.next() else {
            return CallOutcome::Failed;
        };
        let Some(second) = values.next() else {
            return CallOutcome::Failed;
        };
        CallOutcome::Value(IrValue::Pair(IrPair {
            first: Box::new(first),
            second: Box::new(second),
            span: *span,
        }))
    }

    /// Evaluates `.dictionary` from the already parsed Markdown list body.
    /// Entry evaluation is collected privately and published only after all
    /// entries succeed, preserving atomic materialization and source order.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_dictionary(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if !positional_args.is_empty() {
            diagnostics.push(iteration_error(
                "`.dictionary` accepts its entries as a block body".to_string(),
                *span,
            ));
            return CallOutcome::Failed;
        }
        if let Some(argument) = named_args.first() {
            diagnostics.push(iteration_error_at(
                format!(
                    "Unknown named argument `{}` for `.dictionary`",
                    argument.name
                ),
                argument.name_span,
            ));
            return CallOutcome::Failed;
        }
        let body = match body {
            None => &[][..],
            Some(CallBody::Block(nodes)) => nodes,
            Some(CallBody::Inline(_)) => {
                diagnostics.push(iteration_error(
                    "`.dictionary` requires a Markdown list block body".to_string(),
                    *span,
                ));
                return CallOutcome::Failed;
            }
        };
        let entries = match self.evaluate_dictionary_entries(body, *span, diagnostics, context) {
            Ok(entries) => entries,
            Err(outcome) => return outcome,
        };
        CallOutcome::Value(IrValue::Dictionary(IrDictionary {
            entries,
            span: *span,
        }))
    }

    fn evaluate_dictionary_entries(
        &self,
        nodes: &[IrNode],
        span: SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<Vec<IrPair>, CallOutcome> {
        let list = match nodes {
            [] => return Ok(Vec::new()),
            [IrNode::UnorderedList { items, .. }] | [IrNode::OrderedList { items, .. }] => items,
            _ => {
                diagnostics.push(iteration_error(
                    "`.dictionary` requires exactly one Markdown list body".to_string(),
                    span,
                ));
                return Err(CallOutcome::Failed);
            }
        };
        self.check_materialized_elements_len(list.len(), span, diagnostics)?;
        let mut entries = Vec::new();
        if let Err(error) = entries.try_reserve_exact(list.len()) {
            diagnostics.push(iteration_error(
                format!("dictionary entries cannot be allocated: {error}"),
                span,
            ));
            return Err(CallOutcome::Failed);
        }
        for item in list {
            let (key, value) = self.dictionary_item_parts(item, span, diagnostics, context)?;
            let pair = IrPair {
                first: Box::new(IrValue::String(key.clone())),
                second: Box::new(value),
                span: item.span,
            };
            if let Some(existing) = entries.iter_mut().find(|entry: &&mut IrPair| {
                matches!(entry.first.as_ref(), IrValue::String(existing_key) if existing_key == &key)
            }) {
                // Quarkdown's last-write-wins behavior replaces the value in
                // the original insertion slot, keeping iteration deterministic.
                *existing = pair;
            } else {
                entries.push(pair);
            }
        }
        Ok(entries)
    }

    fn dictionary_item_parts(
        &self,
        item: &IrListItem,
        fallback_span: SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<(String, IrValue), CallOutcome> {
        let Some(IrNode::Paragraph { content, span }) = item.nodes.first() else {
            diagnostics.push(iteration_error(
                "Dictionary entries must start with a Markdown paragraph".to_string(),
                item.span,
            ));
            return Err(CallOutcome::Failed);
        };
        let (key, value_inlines, value_text, value_span) =
            if let Some(parts) = split_dictionary_paragraph(content, *span) {
                parts
            } else if item.nodes.len() > 1 {
                let Some(key) = plain_dictionary_key(content) else {
                    diagnostics.push(iteration_error(
                        "Dictionary entries require a string key".to_string(),
                        item.span,
                    ));
                    return Err(CallOutcome::Failed);
                };
                (key, Vec::new(), String::new(), *span)
            } else {
                diagnostics.push(iteration_error(
                    "Dictionary entries require a string key followed by `:`".to_string(),
                    item.span,
                ));
                return Err(CallOutcome::Failed);
            };
        if key.is_empty() {
            diagnostics.push(iteration_error(
                "Dictionary keys must not be empty".to_string(),
                item.span,
            ));
            return Err(CallOutcome::Failed);
        }

        let value = if value_inlines.is_empty() && value_text.is_empty() {
            let nested = &item.nodes[1..];
            if nested.is_empty() {
                IrValue::String(String::new())
            } else {
                let nested =
                    self.evaluate_dictionary_entries(nested, item.span, diagnostics, context)?;
                IrValue::Dictionary(IrDictionary {
                    entries: nested,
                    span: item.span,
                })
            }
        } else if value_inlines.is_empty() {
            dictionary_scalar_value(&value_text)
        } else {
            let value = dictionary_inline_value(value_inlines, value_span);
            match self.evaluate_value(&value, diagnostics, context) {
                CallOutcome::Value(value) => value,
                CallOutcome::Unresolved => {
                    self.preserve_value_expression(&value, diagnostics, context)?
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(&value, &fallback_span)));
                    return Err(CallOutcome::Failed);
                }
                CallOutcome::Failed => return Err(CallOutcome::Failed),
            }
        };
        Ok((key, value))
    }

    /// Evaluates block-form `.let` as a scoped one-argument lambda
    /// invocation. The value is resolved in the caller context exactly once;
    /// only then is the invocation-local child scope created and populated.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_let(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if positional_args.len() != 1 {
            diagnostics.push(let_error(
                format!(
                    "`.let` requires exactly one positional value argument (received {})",
                    positional_args.len()
                ),
                *span,
            ));
            return CallOutcome::Failed;
        }
        if let Some(argument) = named_args.first() {
            diagnostics.push(let_error_at(
                format!("Unknown named argument `{}` for `.let`", argument.name),
                argument.name_span,
            ));
            return CallOutcome::Failed;
        }

        let body = match body {
            Some(CallBody::Block(nodes)) => nodes,
            Some(CallBody::Inline(_)) => {
                diagnostics.push(let_error(
                    "`.let` supports only the block lambda form".to_string(),
                    *span,
                ));
                return CallOutcome::Failed;
            }
            None => {
                diagnostics.push(let_error(
                    "`.let` requires a block lambda body".to_string(),
                    *span,
                ));
                return CallOutcome::Failed;
            }
        };

        if let Some(parameters) = lambda_parameters {
            if parameters.len() != 1 {
                let parameter_span = parameters
                    .first()
                    .map(|parameter| parameter.span)
                    .unwrap_or(*span);
                diagnostics.push(let_error_at(
                    format!(
                        "`.let` requires exactly one explicit lambda parameter (received {})",
                        parameters.len()
                    ),
                    parameter_span,
                ));
                return CallOutcome::Failed;
            }
        }

        let value = match self.evaluate_value(&positional_args[0], diagnostics, context) {
            CallOutcome::Value(value) => value,
            CallOutcome::Unresolved => {
                match self.preserve_value_expression(&positional_args[0], diagnostics, context) {
                    Ok(value) => value,
                    Err(outcome) => return outcome,
                }
            }
            CallOutcome::NoValue => {
                diagnostics.push(no_value_required(value_source_span(
                    &positional_args[0],
                    span,
                )));
                return CallOutcome::Failed;
            }
            CallOutcome::Failed => return CallOutcome::Failed,
        };

        self.invoke_scoped_lambda(
            value,
            lambda_parameters,
            body,
            IterationOptions {
                span: *span,
                allow_destructuring: false,
            },
            diagnostics,
            context,
        )
    }

    /// Evaluates block-form `.foreach` as a typed map over one iterable.
    /// The iterable is resolved before any child scope is created and exactly
    /// once; every mapped element gets a fresh invocation-local child scope.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_foreach(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> CallOutcome {
        if positional_args.len() != 1 {
            diagnostics.push(iteration_error(
                format!(
                    "`.foreach` requires exactly one positional iterable argument (received {})",
                    positional_args.len()
                ),
                *span,
            ));
            return CallOutcome::Failed;
        }
        if let Some(argument) = named_args.first() {
            diagnostics.push(iteration_error_at(
                format!("Unknown named argument `{}` for `.foreach`", argument.name),
                argument.name_span,
            ));
            return CallOutcome::Failed;
        }
        let body = match body {
            Some(CallBody::Block(nodes)) => nodes,
            Some(CallBody::Inline(_)) => {
                diagnostics.push(iteration_error(
                    "`.foreach` supports only the block lambda form in this slice".to_string(),
                    *span,
                ));
                return CallOutcome::Failed;
            }
            None => {
                diagnostics.push(iteration_error(
                    "`.foreach` requires a block lambda body".to_string(),
                    *span,
                ));
                return CallOutcome::Failed;
            }
        };
        if !validate_iteration_lambda(lambda_parameters, ".foreach", true, span, diagnostics) {
            return CallOutcome::Failed;
        }

        let value = match self
            .evaluate_invocation_values(
                std::slice::from_ref(&positional_args[0]),
                span,
                diagnostics,
                context,
                first_origin,
            )
            .and_then(|mut values| values.pop().ok_or(CallOutcome::Failed))
        {
            Ok(value) => value,
            Err(outcome) => return outcome,
        };
        let elements = match self.coerce_iterable(value, span, diagnostics) {
            Ok(elements) => elements,
            Err(outcome) => return outcome,
        };
        self.map_iteration_values(
            &elements,
            lambda_parameters,
            body,
            IterationOptions {
                span: *span,
                allow_destructuring: true,
            },
            diagnostics,
            context,
        )
    }

    /// Evaluates `.repeat` through the same iteration engine as `.foreach`.
    /// The count is a checked semantic integer, and indices are one-based.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_repeat(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if positional_args.len() != 1 {
            diagnostics.push(iteration_error(
                format!(
                    "`.repeat` requires exactly one positional count argument (received {})",
                    positional_args.len()
                ),
                *span,
            ));
            return CallOutcome::Failed;
        }
        if let Some(argument) = named_args.first() {
            diagnostics.push(iteration_error_at(
                format!("Unknown named argument `{}` for `.repeat`", argument.name),
                argument.name_span,
            ));
            return CallOutcome::Failed;
        }
        let body = match body {
            Some(CallBody::Block(nodes)) => nodes,
            Some(CallBody::Inline(_)) => {
                diagnostics.push(iteration_error(
                    "`.repeat` supports only the block lambda form in this slice".to_string(),
                    *span,
                ));
                return CallOutcome::Failed;
            }
            None => {
                diagnostics.push(iteration_error(
                    "`.repeat` requires a block lambda body".to_string(),
                    *span,
                ));
                return CallOutcome::Failed;
            }
        };
        if !validate_iteration_lambda(lambda_parameters, ".repeat", false, span, diagnostics) {
            return CallOutcome::Failed;
        }

        let count_value = match self.evaluate_value(&positional_args[0], diagnostics, context) {
            CallOutcome::Value(value) => value,
            CallOutcome::Unresolved => {
                match self.preserve_value_expression(&positional_args[0], diagnostics, context) {
                    Ok(value) => value,
                    Err(outcome) => return outcome,
                }
            }
            CallOutcome::NoValue => {
                diagnostics.push(no_value_required(value_source_span(
                    &positional_args[0],
                    span,
                )));
                return CallOutcome::Failed;
            }
            CallOutcome::Failed => return CallOutcome::Failed,
        };
        let count = match repeat_count(&count_value) {
            Ok(count) => count,
            Err(message) => {
                diagnostics.push(iteration_error(
                    message,
                    value_source_span(&count_value, span),
                ));
                return CallOutcome::Failed;
            }
        };
        let elements = match self.materialize_closed_range(
            IrRange {
                start: Some(1),
                end: Some(count),
                span: *span,
            },
            span,
            diagnostics,
        ) {
            Ok(elements) => elements,
            Err(outcome) => return outcome,
        };
        self.map_iteration_values(
            &elements,
            lambda_parameters,
            body,
            IterationOptions {
                span: *span,
                allow_destructuring: false,
            },
            diagnostics,
            context,
        )
    }

    /// Evaluates `.map`, `.filter`, and `.sorted` through the same typed
    /// iterable and callable machinery used by `.foreach`.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_collection_transform(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> CallOutcome {
        let (raw_collection, raw_callback) = match transform_operands(
            name,
            positional_args,
            named_args,
            body.is_some(),
            *span,
            diagnostics,
        ) {
            Ok(operands) => operands,
            Err(outcome) => return outcome,
        };
        let collection = match self
            .evaluate_invocation_values(
                std::slice::from_ref(&raw_collection),
                span,
                diagnostics,
                context,
                first_origin,
            )
            .and_then(|mut values| values.pop().ok_or(CallOutcome::Failed))
        {
            Ok(value) => value,
            Err(outcome) => return outcome,
        };
        let elements = match self.coerce_iterable(collection, span, diagnostics) {
            Ok(elements) => elements,
            Err(outcome) => return outcome,
        };

        let callable = match body {
            Some(CallBody::Block(nodes)) => {
                Some(self.make_callable(lambda_parameters, nodes, *span, context))
            }
            Some(CallBody::Inline(_)) => {
                diagnostics.push(iteration_error(
                    format!("`.{name}` requires a block or first-class lambda callback"),
                    *span,
                ));
                return CallOutcome::Failed;
            }
            None => match raw_callback {
                Some(value) => {
                    let value = match self.evaluate_value(&value, diagnostics, context) {
                        CallOutcome::Value(value) => value,
                        CallOutcome::Unresolved => {
                            match self.preserve_value_expression(&value, diagnostics, context) {
                                Ok(value) => value,
                                Err(outcome) => return outcome,
                            }
                        }
                        CallOutcome::NoValue => {
                            diagnostics.push(no_value_required(value_source_span(&value, span)));
                            return CallOutcome::Failed;
                        }
                        CallOutcome::Failed => return CallOutcome::Failed,
                    };
                    match value {
                        IrValue::Callable(callable) => Some(callable),
                        _ => {
                            diagnostics.push(iteration_error(
                                format!("`.{name}` callback must be a first-class callable"),
                                value_source_span(&value, span),
                            ));
                            return CallOutcome::Failed;
                        }
                    }
                }
                None if name == "sorted" => None,
                None => {
                    diagnostics.push(iteration_error(
                        format!("`.{name}` requires a callback lambda"),
                        *span,
                    ));
                    return CallOutcome::Failed;
                }
            },
        };

        match name {
            "map" => {
                let Some(callable) = callable.as_ref() else {
                    diagnostics.push(iteration_error(
                        "`.map` requires a callback lambda".to_string(),
                        *span,
                    ));
                    return CallOutcome::Failed;
                };
                self.map_callable_values(
                    &elements,
                    callable,
                    IterationOptions {
                        span: *span,
                        allow_destructuring: true,
                    },
                    diagnostics,
                    context,
                )
            }
            "filter" => {
                let Some(callable) = callable.as_ref() else {
                    diagnostics.push(iteration_error(
                        "`.filter` requires a predicate lambda".to_string(),
                        *span,
                    ));
                    return CallOutcome::Failed;
                };
                self.filter_callable_values(&elements, callable, *span, diagnostics, context)
            }
            "sorted" => {
                self.sort_iterable_values(elements, callable.as_ref(), *span, diagnostics, context)
            }
            _ => {
                diagnostics.push(iteration_error(
                    format!("Unsupported collection transform `.{name}`"),
                    *span,
                ));
                CallOutcome::Failed
            }
        }
    }

    /// Evaluates the bounded callback-based optionality functions from the
    /// v2.5.1 Optionality module. The value is resolved before a callback is
    /// invoked. `.ifpresent` skips its callback for semantic `None`, while
    /// `.takeif` still invokes its predicate with `None`, matching the
    /// distinct upstream callback contracts.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_optionality_callback(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        lambda_parameters: Option<&[IrParameter]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        let (raw_value, raw_callback) = match optionality_operands(
            name,
            positional_args,
            named_args,
            body.is_some(),
            *span,
            diagnostics,
        ) {
            Ok(operands) => operands,
            Err(outcome) => return outcome,
        };
        let value = match self.evaluate_value(&raw_value, diagnostics, context) {
            CallOutcome::Value(value) => value,
            CallOutcome::Unresolved => {
                match self.preserve_value_expression(&raw_value, diagnostics, context) {
                    Ok(value) => value,
                    Err(outcome) => return outcome,
                }
            }
            CallOutcome::NoValue => {
                diagnostics.push(no_value_required(value_source_span(&raw_value, span)));
                return CallOutcome::Failed;
            }
            CallOutcome::Failed => return CallOutcome::Failed,
        };

        if name == "ifpresent" && matches!(value, IrValue::None) {
            return CallOutcome::Value(IrValue::None);
        }

        let callable = match body {
            Some(CallBody::Block(nodes)) => {
                self.make_callable(lambda_parameters, nodes, *span, context)
            }
            Some(CallBody::Inline(_)) => {
                diagnostics.push(function_error(
                    format!("`.{name}` requires a block or first-class lambda callback"),
                    *span,
                ));
                return CallOutcome::Failed;
            }
            None => {
                let Some(raw_callback) = raw_callback else {
                    diagnostics.push(function_error(
                        format!("`.{name}` requires a callback lambda"),
                        *span,
                    ));
                    return CallOutcome::Failed;
                };
                let callback = match self.evaluate_value(&raw_callback, diagnostics, context) {
                    CallOutcome::Value(IrValue::Callable(callable)) => callable,
                    CallOutcome::Value(value) => {
                        diagnostics.push(iteration_error(
                            format!("`.{name}` callback must be a first-class callable"),
                            value_source_span(&value, span),
                        ));
                        return CallOutcome::Failed;
                    }
                    CallOutcome::Unresolved => {
                        diagnostics.push(iteration_error(
                            format!("`.{name}` callback must be a first-class callable"),
                            value_source_span(&raw_callback, span),
                        ));
                        return CallOutcome::Failed;
                    }
                    CallOutcome::NoValue => {
                        diagnostics.push(no_value_required(value_source_span(&raw_callback, span)));
                        return CallOutcome::Failed;
                    }
                    CallOutcome::Failed => return CallOutcome::Failed,
                };
                callback
            }
        };
        let callback_result = match self.invoke_callable(
            &callable,
            vec![value.clone()],
            IterationOptions {
                span: *span,
                allow_destructuring: false,
            },
            diagnostics,
            context,
        ) {
            CallOutcome::Value(value) => value,
            CallOutcome::NoValue => {
                diagnostics.push(no_value_required(callable.span));
                return CallOutcome::Failed;
            }
            CallOutcome::Failed => return CallOutcome::Failed,
            CallOutcome::Unresolved => return CallOutcome::Unresolved,
        };

        if name == "ifpresent" {
            return CallOutcome::Value(callback_result);
        }

        let Some(condition) = scalar_boolean_value(&callback_result) else {
            diagnostics.push(iteration_error(
                "`.takeif` condition must return Boolean".to_string(),
                value_source_span(&callback_result, &callable.span),
            ));
            return CallOutcome::Failed;
        };
        if condition {
            CallOutcome::Value(value)
        } else {
            CallOutcome::Value(IrValue::None)
        }
    }

    /// Evaluates `.range` into the same typed Range representation used by
    /// literal range values. Bounds are evaluated through the ordinary value
    /// path before the upstream Number-to-Int-compatible conversion.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_range(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> CallOutcome {
        if body.is_some() {
            diagnostics.push(iteration_error(
                "`.range` does not accept a block body".to_string(),
                *span,
            ));
            return CallOutcome::Failed;
        }
        let (start, end) = match range_arguments(positional_args, named_args, span, diagnostics) {
            Ok(arguments) => arguments,
            Err(outcome) => return outcome,
        };
        let start = match start {
            Some(value) => {
                match self.evaluate_range_endpoint(&value, span, diagnostics, context, first_origin)
                {
                    Ok(value) => Some(value),
                    Err(outcome) => return outcome,
                }
            }
            None => None,
        };
        let end = match end {
            Some(value) => {
                match self.evaluate_range_endpoint(&value, span, diagnostics, context, None) {
                    Ok(value) => Some(value),
                    Err(outcome) => return outcome,
                }
            }
            None => None,
        };
        CallOutcome::Value(IrValue::Range(IrRange {
            start,
            end,
            span: *span,
        }))
    }

    fn evaluate_range_endpoint(
        &self,
        value: &IrValue,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        origin: Option<ValueOrigin>,
    ) -> Result<i32, CallOutcome> {
        let evaluated = self
            .evaluate_invocation_values(
                std::slice::from_ref(value),
                span,
                diagnostics,
                context,
                origin,
            )?
            .into_iter()
            .next()
            .ok_or(CallOutcome::Failed)?;
        number_to_range_endpoint(&evaluated).map_err(|message| {
            diagnostics.push(iteration_error(message, value_source_span(value, span)));
            CallOutcome::Failed
        })
    }

    fn invoke_scoped_lambda(
        &self,
        value: IrValue,
        lambda_parameters: Option<&[IrParameter]>,
        body: &[IrNode],
        options: IterationOptions,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        let callable = self.make_callable(lambda_parameters, body, options.span, context);
        self.invoke_callable(&callable, vec![value], options, diagnostics, context)
    }

    fn make_callable(
        &self,
        parameters: Option<&[IrParameter]>,
        body: &[IrNode],
        span: SourceSpan,
        context: &EvaluationContext<'_>,
    ) -> IrCallable {
        IrCallable {
            parameters: parameters.map(ToOwned::to_owned),
            body: body.to_vec(),
            span,
            capture: Some(Box::new(context.capture_snapshot())),
        }
    }

    /// Shared first-class callable invocation path for loops, transforms, and
    /// user-defined callables. Invocation never mutates the caller context.
    fn invoke_callable(
        &self,
        callable: &IrCallable,
        arguments: Vec<IrValue>,
        options: IterationOptions,
        diagnostics: &mut Vec<Diagnostic>,
        caller_context: &EvaluationContext<'_>,
    ) -> CallOutcome {
        let _depth = match caller_context.enter_evaluation_depth(options.span, diagnostics) {
            Ok(depth) => depth,
            Err(outcome) => return outcome,
        };
        let bound = match bind_invocation_arguments(
            callable.parameters.as_deref(),
            arguments,
            options.allow_destructuring,
            options.span,
            diagnostics,
        ) {
            Ok(bound) => bound,
            Err(outcome) => return outcome,
        };
        self.invoke_bound_callable(callable, bound, options, diagnostics, caller_context)
    }

    fn invoke_bound_callable(
        &self,
        callable: &IrCallable,
        bound: BoundLambdaArguments,
        _options: IterationOptions,
        diagnostics: &mut Vec<Diagnostic>,
        caller_context: &EvaluationContext<'_>,
    ) -> CallOutcome {
        let definition_context = callable
            .capture
            .as_deref()
            .map(EvaluationContext::from_capture)
            .unwrap_or_else(EvaluationContext::new);
        // Preserve the definition snapshot as the lexical base, then add only
        // caller-visible lookup bindings. Invocation parameters are installed
        // in the child below, after both layers, so they have highest
        // precedence. Document state is shared separately by the overlay.
        let invocation_base =
            EvaluationContext::with_caller_overlay(definition_context, caller_context);
        let mut child = invocation_base.child();
        match bound {
            BoundLambdaArguments::Explicit(values) => {
                child.set_lambda_scope(LambdaScope::Explicit);
                if let Some(parameters) = callable.parameters.as_deref() {
                    for (parameter, value) in parameters.iter().zip(values) {
                        child.set_value(parameter.name.clone(), value);
                    }
                }
            }
            BoundLambdaArguments::Implicit(values) => {
                child.set_lambda_scope(LambdaScope::Implicit(values));
            }
        }
        self.evaluate_callable_body_value(&callable.body, diagnostics, &mut child)
    }

    fn map_iteration_values(
        &self,
        elements: &[IrValue],
        lambda_parameters: Option<&[IrParameter]>,
        body: &[IrNode],
        options: IterationOptions,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        let callable = self.make_callable(lambda_parameters, body, options.span, context);
        self.map_callable_values(elements, &callable, options, diagnostics, context)
    }

    fn map_callable_values(
        &self,
        elements: &[IrValue],
        callable: &IrCallable,
        options: IterationOptions,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if let Err(outcome) =
            self.check_materialized_elements_len(elements.len(), options.span, diagnostics)
        {
            return outcome;
        }
        let mut results = Vec::new();
        if let Err(error) = results.try_reserve_exact(elements.len()) {
            diagnostics.push(iteration_error(
                format!("iteration result collection cannot be allocated: {error}"),
                options.span,
            ));
            return CallOutcome::Failed;
        }
        for element in elements {
            match self.invoke_callable(
                callable,
                vec![element.clone()],
                options,
                diagnostics,
                context,
            ) {
                CallOutcome::Value(value) => results.push(value),
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(options.span));
                    return CallOutcome::Failed;
                }
                CallOutcome::Failed => return CallOutcome::Failed,
                CallOutcome::Unresolved => return CallOutcome::Unresolved,
            }
        }
        CallOutcome::Value(IrValue::Collection(results))
    }

    fn filter_callable_values(
        &self,
        elements: &[IrValue],
        callable: &IrCallable,
        span: SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if let Err(outcome) =
            self.check_materialized_elements_len(elements.len(), span, diagnostics)
        {
            return outcome;
        }
        let mut results = Vec::new();
        if let Err(error) = results.try_reserve_exact(elements.len()) {
            diagnostics.push(iteration_error(
                format!("filter result collection cannot be allocated: {error}"),
                span,
            ));
            return CallOutcome::Failed;
        }
        for element in elements {
            let predicate = match self.invoke_callable(
                callable,
                vec![element.clone()],
                IterationOptions {
                    span,
                    allow_destructuring: true,
                },
                diagnostics,
                context,
            ) {
                CallOutcome::Value(value) => value,
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(callable.span));
                    return CallOutcome::Failed;
                }
                CallOutcome::Failed => return CallOutcome::Failed,
                CallOutcome::Unresolved => return CallOutcome::Unresolved,
            };
            let Some(keep) = scalar_boolean_value(&predicate) else {
                diagnostics.push(iteration_error(
                    "`.filter` predicate must return Boolean".to_string(),
                    value_source_span(&predicate, &callable.span),
                ));
                return CallOutcome::Failed;
            };
            if keep {
                results.push(element.clone());
            }
        }
        CallOutcome::Value(IrValue::Collection(results))
    }

    fn sort_iterable_values(
        &self,
        elements: Vec<IrValue>,
        callable: Option<&IrCallable>,
        span: SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if let Err(outcome) =
            self.check_materialized_elements_len(elements.len(), span, diagnostics)
        {
            return outcome;
        }
        let mut keyed = Vec::new();
        if let Err(error) = keyed.try_reserve_exact(elements.len()) {
            diagnostics.push(iteration_error(
                format!("sorted collection cannot be allocated: {error}"),
                span,
            ));
            return CallOutcome::Failed;
        }
        for element in elements {
            let key = match callable {
                Some(callable) => match self.invoke_callable(
                    callable,
                    vec![element.clone()],
                    IterationOptions {
                        span,
                        allow_destructuring: true,
                    },
                    diagnostics,
                    context,
                ) {
                    CallOutcome::Value(value) => value,
                    CallOutcome::NoValue => {
                        diagnostics.push(no_value_required(callable.span));
                        return CallOutcome::Failed;
                    }
                    CallOutcome::Failed => return CallOutcome::Failed,
                    CallOutcome::Unresolved => return CallOutcome::Unresolved,
                },
                None => element.clone(),
            };
            let key = match SortKey::try_from_value(&key) {
                Ok(key) => key,
                Err(message) => {
                    diagnostics.push(iteration_error(message, value_source_span(&key, &span)));
                    return CallOutcome::Failed;
                }
            };
            keyed.push((element, key));
        }
        if let Some((_, first_key)) = keyed.first() {
            if keyed
                .iter()
                .skip(1)
                .any(|(_, key)| !first_key.same_kind(key))
            {
                diagnostics.push(iteration_error(
                    "`.sorted` does not compare heterogeneous key types".to_string(),
                    span,
                ));
                return CallOutcome::Failed;
            }
        }
        keyed.sort_by(|(_, left), (_, right)| left.cmp(right));
        let mut sorted = Vec::new();
        if let Err(error) = sorted.try_reserve_exact(keyed.len()) {
            diagnostics.push(iteration_error(
                format!("sorted result collection cannot be allocated: {error}"),
                span,
            ));
            return CallOutcome::Failed;
        }
        sorted.extend(keyed.into_iter().map(|(value, _)| value));
        CallOutcome::Value(IrValue::Collection(sorted))
    }

    fn coerce_iterable(
        &self,
        value: InvocationValue,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Vec<IrValue>, CallOutcome> {
        let InvocationValue { value, origin } = value;
        match value {
            IrValue::Collection(values) => {
                self.check_materialized_elements_len(values.len(), *span, diagnostics)?;
                Ok(values)
            }
            IrValue::Pair(pair) => {
                self.check_materialized_elements_len(2, pair.span, diagnostics)?;
                let mut values = Vec::new();
                if let Err(error) = values.try_reserve_exact(2) {
                    diagnostics.push(iteration_error(
                        format!("Pair iterable cannot be allocated: {error}"),
                        pair.span,
                    ));
                    return Err(CallOutcome::Failed);
                }
                values.push(*pair.first);
                values.push(*pair.second);
                Ok(values)
            }
            IrValue::Dictionary(dictionary) => {
                self.check_materialized_elements_len(
                    dictionary.entries.len(),
                    dictionary.span,
                    diagnostics,
                )?;
                let mut values = Vec::new();
                if let Err(error) = values.try_reserve_exact(dictionary.entries.len()) {
                    diagnostics.push(iteration_error(
                        format!("Dictionary iterable cannot be allocated: {error}"),
                        dictionary.span,
                    ));
                    return Err(CallOutcome::Failed);
                }
                values.extend(dictionary.entries.into_iter().map(IrValue::Pair));
                Ok(values)
            }
            IrValue::Range(range) => self.materialize_range(range, span, diagnostics),
            value @ (IrValue::String(_) | IrValue::Identifier(_)) => {
                let argument = InvocationValue { value, origin };
                match value_conversion::convert_range_with_origin(&argument, *span) {
                    Ok(range) => self.materialize_range(range, span, diagnostics),
                    Err(_) => {
                        diagnostics.push(iteration_error(
                            "Value is not an iterable Range, Collection, Pair, Dictionary, or exactly one Markdown list"
                                .to_string(),
                            *span,
                        ));
                        Err(CallOutcome::Failed)
                    }
                }
            }
            IrValue::Content(nodes) => match nodes.as_slice() {
                [IrNode::UnorderedList { items, .. }] | [IrNode::OrderedList { items, .. }] => {
                    self.check_materialized_elements_len(items.len(), *span, diagnostics)?;
                    let mut values = Vec::new();
                    if let Err(error) = values.try_reserve_exact(items.len()) {
                        diagnostics.push(iteration_error(
                            format!("list collection cannot be allocated: {error}"),
                            *span,
                        ));
                        return Err(CallOutcome::Failed);
                    }
                    for item in items {
                        values.push(self.list_item_value(item, span, diagnostics)?);
                    }
                    Ok(values)
                }
                _ => {
                    diagnostics.push(iteration_error(
                        "Value is not an iterable Range, Collection, Pair, Dictionary, or exactly one Markdown list"
                            .to_string(),
                        *span,
                    ));
                    Err(CallOutcome::Failed)
                }
            },
            _ => {
                diagnostics.push(iteration_error(
                    "Value is not an iterable Range, Collection, Pair, Dictionary, or exactly one Markdown list"
                        .to_string(),
                    *span,
                ));
                Err(CallOutcome::Failed)
            }
        }
    }

    fn list_item_value(
        &self,
        item: &scribium_ir::IrListItem,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<IrValue, CallOutcome> {
        match item.nodes.as_slice() {
            [IrNode::UnorderedList { .. }] | [IrNode::OrderedList { .. }] => self
                .coerce_iterable(
                    InvocationValue::static_value(IrValue::Content(item.nodes.clone())),
                    span,
                    diagnostics,
                )
                .map(IrValue::Collection),
            [IrNode::Paragraph { content, .. }] => match content.as_slice() {
                [IrInline::Text { content, .. }] => Ok(IrValue::String(content.clone())),
                _ => Ok(IrValue::Content(item.nodes.clone())),
            },
            _ => Ok(IrValue::Content(item.nodes.clone())),
        }
    }

    fn check_materialized_elements(
        &self,
        requested: u64,
        span: SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<usize, CallOutcome> {
        let limit = self.limits.max_materialized_elements as u64;
        if requested > limit {
            diagnostics.push(materialized_elements_limit_error(
                requested,
                self.limits.max_materialized_elements,
                span,
            ));
            return Err(CallOutcome::Failed);
        }
        usize::try_from(requested).map_err(|_| {
            diagnostics.push(iteration_error(
                "Materialized element count is too large for this target".to_string(),
                span,
            ));
            CallOutcome::Failed
        })
    }

    fn check_materialized_elements_len(
        &self,
        requested: usize,
        span: SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<(), CallOutcome> {
        self.check_materialized_elements(requested as u64, span, diagnostics)
            .map(|_| ())
    }

    fn materialize_range(
        &self,
        range: IrRange,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Vec<IrValue>, CallOutcome> {
        let Some(end) = range.end else {
            diagnostics.push(iteration_error(
                "Cannot iterate through an endless Range".to_string(),
                range.span,
            ));
            return Err(CallOutcome::Failed);
        };
        let start = range.start.unwrap_or(1);
        self.materialize_closed_range(
            IrRange {
                start: Some(start),
                end: Some(end),
                span: range.span,
            },
            span,
            diagnostics,
        )
    }

    fn materialize_closed_range(
        &self,
        range: IrRange,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Vec<IrValue>, CallOutcome> {
        let (Some(start), Some(end)) = (range.start, range.end) else {
            diagnostics.push(iteration_error(
                "Internal error: a closed range requires both endpoints".to_string(),
                *span,
            ));
            return Err(CallOutcome::Failed);
        };
        if start > end {
            // Verified against Quarkdown v2.5.1: Range(4, 2) delegates to
            // Kotlin IntRange(4, 2), whose iterator is empty.
            return Ok(Vec::new());
        }
        let Some(count) = i64::from(end)
            .checked_sub(i64::from(start))
            .and_then(|distance| distance.checked_add(1))
        else {
            diagnostics.push(iteration_error(
                "Closed Range cardinality overflowed the supported integer domain".to_string(),
                range.span,
            ));
            return Err(CallOutcome::Failed);
        };
        let capacity = self.check_materialized_elements(count as u64, range.span, diagnostics)?;
        let mut values = Vec::new();
        if let Err(error) = values.try_reserve_exact(capacity) {
            diagnostics.push(iteration_error(
                format!("Closed Range cannot be materialized: {error}"),
                range.span,
            ));
            return Err(CallOutcome::Failed);
        }
        let mut current = start;
        loop {
            values.push(IrValue::Number(current as f64));
            if current == end {
                break;
            }
            current = match current.checked_add(1) {
                Some(next) => next,
                None => {
                    diagnostics.push(iteration_error(
                        "Closed Range iteration overflowed its endpoint".to_string(),
                        range.span,
                    ));
                    return Err(CallOutcome::Failed);
                }
            };
        }
        Ok(values)
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_user_function(
        &self,
        binding: &FunctionBinding,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        // Caller arguments are evaluated before any callee scope is created.
        // The parser guarantees that positional arguments precede named ones,
        // so these two passes preserve source order for the supported grammar.
        let positional = match self.evaluate_values(positional_args, span, diagnostics, context) {
            Ok(values) => values,
            Err(outcome) => return outcome,
        };
        let named = match self.evaluate_named(named_args, span, diagnostics, context) {
            Ok(values) => values,
            Err(outcome) => return outcome,
        };
        let bound = match self.bind_callable_arguments(
            &binding.parameters,
            positional,
            named,
            body,
            span,
            diagnostics,
            context,
        ) {
            Ok(bound) => bound,
            Err(outcome) => return outcome,
        };
        let callable = binding.as_callable();
        self.invoke_bound_callable(
            &callable,
            bound,
            IterationOptions {
                span: *span,
                allow_destructuring: false,
            },
            diagnostics,
            context,
        )
    }

    /// Evaluates and binds one callable's arguments for either parameter mode.
    /// The result is consumed by the shared child-scope/body invocation path.
    #[allow(clippy::too_many_arguments)]
    fn bind_callable_arguments(
        &self,
        parameters: &LambdaParameters,
        positional: Vec<IrValue>,
        named: Vec<IrNamedArg>,
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<BoundLambdaArguments, CallOutcome> {
        match parameters {
            LambdaParameters::Implicit => {
                if let Some(argument) = named.first() {
                    diagnostics.push(function_error_at(
                        "Implicit lambda parameters are positional only".to_string(),
                        argument.name_span,
                    ));
                    return Err(CallOutcome::Failed);
                }
                let mut arguments = positional;
                if let Some(body) = body {
                    let value = match self.evaluate_call_body(body, span, diagnostics, context) {
                        CallOutcome::Value(value) => value,
                        CallOutcome::NoValue => return Err(CallOutcome::NoValue),
                        CallOutcome::Failed => return Err(CallOutcome::Failed),
                        CallOutcome::Unresolved => return Err(CallOutcome::Unresolved),
                    };
                    arguments.push(value);
                }
                Ok(BoundLambdaArguments::Implicit(arguments))
            }
            LambdaParameters::Explicit(parameters) => {
                let mut bound: Vec<Option<IrValue>> = vec![None; parameters.len()];
                for (index, value) in positional.into_iter().enumerate() {
                    let Some(slot) = bound.get_mut(index) else {
                        diagnostics.push(function_error(
                            format!(
                                "Function call has too many positional arguments (received at least {})",
                                index + 1
                            ),
                            *span,
                        ));
                        return Err(CallOutcome::Failed);
                    };
                    *slot = Some(value);
                }

                for argument in &named {
                    let Some(index) = parameters
                        .iter()
                        .position(|parameter| parameter.name == argument.name)
                    else {
                        diagnostics.push(function_error_at(
                            format!("Unknown named parameter `{}`", argument.name),
                            argument.name_span,
                        ));
                        return Err(CallOutcome::Failed);
                    };
                    if bound[index].is_some() {
                        diagnostics.push(function_error_at(
                            format!("Parameter `{}` was bound more than once", argument.name),
                            argument.name_span,
                        ));
                        return Err(CallOutcome::Failed);
                    }
                    bound[index] = Some(argument.value.clone());
                }

                if let Some(body) = body {
                    let Some(last) = bound.last() else {
                        diagnostics.push(function_error(
                            "A block argument requires a final function parameter".to_string(),
                            *span,
                        ));
                        return Err(CallOutcome::Failed);
                    };
                    if last.is_some() {
                        diagnostics.push(function_error(
                            "A block argument collides with the function's final parameter binding"
                                .to_string(),
                            *span,
                        ));
                        return Err(CallOutcome::Failed);
                    }
                    let value = match self.evaluate_call_body(body, span, diagnostics, context) {
                        CallOutcome::Value(value) => value,
                        CallOutcome::NoValue => return Err(CallOutcome::NoValue),
                        CallOutcome::Failed => return Err(CallOutcome::Failed),
                        CallOutcome::Unresolved => return Err(CallOutcome::Unresolved),
                    };
                    if let Some(last) = bound.last_mut() {
                        *last = Some(value);
                    }
                }

                for (index, parameter) in parameters.iter().enumerate() {
                    if bound[index].is_none() {
                        if parameter.optional {
                            bound[index] = Some(IrValue::None);
                        } else {
                            diagnostics.push(function_error_at(
                                format!("Missing required argument `{}`", parameter.name),
                                parameter.name_span,
                            ));
                            return Err(CallOutcome::Failed);
                        }
                    }
                }

                Ok(BoundLambdaArguments::Explicit(
                    bound.into_iter().flatten().collect(),
                ))
            }
        }
    }

    fn evaluate_callable_body_value(
        &self,
        nodes: &[IrNode],
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        let mut result = CallableBodyValueAccumulator::Empty;
        for node in nodes {
            let span = ir_node_source_span(node);
            match self.evaluate_callable_statement_value(node, diagnostics, context) {
                CallOutcome::Value(value) => {
                    if let Err(outcome) = result.append_value(value, span, diagnostics) {
                        return outcome;
                    }
                }
                CallOutcome::NoValue => {}
                CallOutcome::Failed => return CallOutcome::Failed,
                CallOutcome::Unresolved => return CallOutcome::Unresolved,
            }
        }
        result.finish()
    }

    /// Evaluates one callable-body statement in semantic value context.
    ///
    /// Function calls and chains use the same shared dispatch as every other
    /// call site. Markdown nodes are retained as structured content, while
    /// declarations and outputless calls contribute state without becoming a
    /// fabricated empty value.
    fn evaluate_callable_statement_value(
        &self,
        node: &IrNode,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        match node {
            IrNode::FunctionCall {
                name,
                positional_args,
                named_args,
                lambda_parameters,
                body,
                span,
            } => match self.evaluate_call_value(
                name,
                positional_args,
                named_args,
                body.as_deref().map(CallBody::Block),
                lambda_parameters.as_deref(),
                span,
                diagnostics,
                context,
            ) {
                CallOutcome::Unresolved => self
                    .preserve_block_call(
                        name,
                        positional_args,
                        named_args,
                        lambda_parameters.as_deref(),
                        body.as_deref(),
                        span,
                        diagnostics,
                        context,
                    )
                    .map(IrValue::Content)
                    .map_or(CallOutcome::Failed, CallOutcome::Value),
                outcome => outcome,
            },
            IrNode::ChainedFunctionCall {
                head, chain, body, ..
            } => self.evaluate_chain_value(
                head,
                chain,
                body.as_deref().map(CallBody::Block),
                diagnostics,
                context,
            ),
            _ => {
                let before = diagnostics.len();
                let evaluated = self.evaluate_node(node, diagnostics, context);
                if diagnostics.len() != before {
                    CallOutcome::Failed
                } else if evaluated.is_empty() {
                    CallOutcome::NoValue
                } else {
                    CallOutcome::Value(IrValue::Content(evaluated))
                }
            }
        }
    }

    /// Evaluates a chain strictly left-to-right using semantic `IrValue`s.
    fn evaluate_chain_value(
        &self,
        head: &IrCallSegment,
        chain: &[IrCallSegment],
        body: Option<CallBody<'_>>,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        let mut value_origin = call_result_origin(&head.name, context);
        let mut value = match self.chain_outcome(
            self.evaluate_call_value(
                &head.name,
                &head.positional_args,
                &head.named_args,
                None,
                None,
                &head.span,
                diagnostics,
                context,
            ),
            head,
            !chain.is_empty(),
            diagnostics,
            context,
        ) {
            CallOutcome::Value(value) => value,
            outcome => return outcome,
        };

        for (index, source_segment) in chain.iter().enumerate() {
            let mut positional_args = Vec::with_capacity(1 + source_segment.positional_args.len());
            // The previous result is always first. Explicit positional
            // arguments follow it in their original order; named arguments
            // remain in the named collection untouched.
            positional_args.push(value);
            positional_args.extend(source_segment.positional_args.iter().cloned());
            let final_body = (index + 1 == chain.len()).then_some(body).flatten();
            let outcome = self.chain_outcome(
                self.evaluate_call_value_with_first_origin(
                    &source_segment.name,
                    &positional_args,
                    &source_segment.named_args,
                    final_body,
                    None,
                    &source_segment.span,
                    diagnostics,
                    context,
                    Some(value_origin),
                ),
                source_segment,
                index + 1 < chain.len(),
                diagnostics,
                context,
            );
            match outcome {
                CallOutcome::Value(next_value) => {
                    value = next_value;
                    value_origin = call_result_origin(&source_segment.name, context);
                }
                outcome => return outcome,
            }
        }

        CallOutcome::Value(value)
    }

    fn chain_outcome(
        &self,
        outcome: CallOutcome,
        segment: &IrCallSegment,
        value_required: bool,
        diagnostics: &mut Vec<Diagnostic>,
        context: &EvaluationContext<'_>,
    ) -> CallOutcome {
        match outcome {
            CallOutcome::Value(value) => CallOutcome::Value(value),
            CallOutcome::NoValue if value_required => {
                diagnostics.push(chain_evaluation_error(
                    format!(
                        "Chained call segment `.{}` produced no value required by a later segment",
                        segment.name
                    ),
                    segment.name_span,
                ));
                CallOutcome::Failed
            }
            CallOutcome::NoValue => CallOutcome::NoValue,
            CallOutcome::Failed => CallOutcome::Failed,
            CallOutcome::Unresolved => {
                let message = if let Some(binding) = context.get_function(&segment.name) {
                    format!(
                        "Function `{}` is visible in this scope but callable function declarations are not implemented yet ({})",
                        segment.name,
                        binding.parameters.description()
                    )
                } else {
                    format!(
                        "Cannot evaluate chained call segment `.{}`: no semantic implementation is available",
                        segment.name
                    )
                };
                diagnostics.push(chain_evaluation_error(message, segment.name_span));
                CallOutcome::Failed
            }
        }
    }

    /// Evaluates a call body only after its callee has selected that strategy.
    fn evaluate_call_body(
        &self,
        body: CallBody<'_>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        let before = diagnostics.len();
        let value = match body {
            CallBody::Block(nodes) => {
                IrValue::Content(self.evaluate_nodes(nodes, diagnostics, context))
            }
            CallBody::Inline(inlines) => IrValue::Content(vec![IrNode::Paragraph {
                content: self.evaluate_inlines(inlines, diagnostics, context),
                span: *span,
            }]),
        };
        if diagnostics.len() == before {
            CallOutcome::Value(value)
        } else {
            CallOutcome::Failed
        }
    }

    /// Resolves only a conditional's condition. Body and content arguments
    /// remain lazy until the branch is known.
    #[allow(clippy::too_many_arguments)]
    fn resolve_call_condition(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> Result<bool, CallOutcome> {
        let Some((raw_condition, condition_origin)) = named_args
            .iter()
            .find(|arg| arg.name == "condition")
            .map(|arg| (&arg.value, None))
            .or_else(|| positional_args.first().map(|value| (value, first_origin)))
        else {
            diagnostics.push(unresolvable_condition(name, span));
            return Err(CallOutcome::Failed);
        };
        let condition = if let IrValue::Identifier(name) = raw_condition {
            context
                .get(name)
                .map(|value| InvocationValue::dynamic_value(value.to_value()))
                .unwrap_or_else(|| InvocationValue {
                    value: raw_condition.clone(),
                    origin: first_origin.unwrap_or(ValueOrigin::Dynamic),
                })
        } else {
            let Some(condition) = self
                .evaluate_invocation_values(
                    std::slice::from_ref(raw_condition),
                    span,
                    diagnostics,
                    context,
                    condition_origin,
                )?
                .into_iter()
                .next()
            else {
                diagnostics.push(unresolvable_condition(name, span));
                return Err(CallOutcome::Failed);
            };
            condition
        };
        match resolve_boolean_value(&condition) {
            Some(value) => Ok(value),
            None => {
                diagnostics.push(unresolvable_condition(name, span));
                Err(CallOutcome::Failed)
            }
        }
    }

    /// Produces conditional content after the condition has selected the
    /// branch. The body and body-like arguments are evaluated here, not before
    /// dispatch.
    fn conditional_content_value(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        if let Some(body) = body {
            return self.evaluate_call_body(body, span, diagnostics, context);
        }
        if let Some(arg) = named_args.iter().find(|arg| arg.name == "body") {
            let value = &arg.value;
            return self.evaluate_content_argument(value, span, diagnostics, context);
        }
        if let Some(value) = positional_args.get(1) {
            return self.evaluate_content_argument(value, span, diagnostics, context);
        }
        CallOutcome::Value(IrValue::Content(Vec::new()))
    }

    fn evaluate_content_argument(
        &self,
        value: &IrValue,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        match self.evaluate_value(value, diagnostics, context) {
            CallOutcome::Value(value) => match self.scalar_or_content(value, span, diagnostics) {
                Ok(value) => CallOutcome::Value(value),
                Err(outcome) => outcome,
            },
            CallOutcome::NoValue => {
                diagnostics.push(no_value_required(value_source_span(value, span)));
                CallOutcome::Failed
            }
            CallOutcome::Failed => CallOutcome::Failed,
            CallOutcome::Unresolved => {
                match self.preserve_value_expression(value, diagnostics, context) {
                    Ok(value) => match self.scalar_or_content(value, span, diagnostics) {
                        Ok(value) => CallOutcome::Value(value),
                        Err(outcome) => outcome,
                    },
                    Err(outcome) => outcome,
                }
            }
        }
    }

    fn scalar_or_content(
        &self,
        value: IrValue,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<IrValue, CallOutcome> {
        match value {
            IrValue::Content(nodes) => Ok(IrValue::Content(nodes)),
            value => value_into_content_nodes(value, *span, diagnostics).map(IrValue::Content),
        }
    }

    fn validate_preserved_value(
        &self,
        value: &IrValue,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<(), CallOutcome> {
        match value {
            IrValue::Range(range) => {
                diagnostics.push(iteration_error(
                    "A typed Range cannot be preserved as an unresolved call argument; consume it through iteration first"
                        .to_string(),
                    range.span,
                ));
                Err(CallOutcome::Failed)
            }
            IrValue::Collection(values) => {
                for value in values {
                    self.validate_preserved_value(value, diagnostics)?;
                }
                Ok(())
            }
            IrValue::Pair(pair) => {
                self.validate_preserved_value(&pair.first, diagnostics)?;
                self.validate_preserved_value(&pair.second, diagnostics)
            }
            IrValue::Dictionary(dictionary) => {
                for pair in &dictionary.entries {
                    self.validate_preserved_value(&pair.first, diagnostics)?;
                    self.validate_preserved_value(&pair.second, diagnostics)?;
                }
                Ok(())
            }
            IrValue::Callable(callable) => {
                diagnostics.push(iteration_error(
                    "A callable cannot be preserved as an unresolved call argument".to_string(),
                    callable.span,
                ));
                Err(CallOutcome::Failed)
            }
            _ => Ok(()),
        }
    }

    /// Retains an unresolved value expression without turning it into an
    /// empty successful value. Its nested arguments still run through the
    /// value-required preservation path, so failures cannot be erased.
    fn preserve_value_expression(
        &self,
        value: &IrValue,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<IrValue, CallOutcome> {
        match value {
            IrValue::Content(nodes) => {
                if let [IrNode::FunctionCall {
                    name,
                    positional_args,
                    named_args,
                    lambda_parameters,
                    body,
                    span,
                }] = nodes.as_slice()
                {
                    return self
                        .preserve_block_call(
                            name,
                            positional_args,
                            named_args,
                            lambda_parameters.as_deref(),
                            body.as_deref(),
                            span,
                            diagnostics,
                            context,
                        )
                        .map(IrValue::Content);
                }
                let before = diagnostics.len();
                let nodes = self.evaluate_nodes(nodes, diagnostics, context);
                if diagnostics.len() == before {
                    Ok(IrValue::Content(nodes))
                } else {
                    Err(CallOutcome::Failed)
                }
            }
            scalar => {
                self.validate_preserved_value(scalar, diagnostics)?;
                Ok(scalar.clone())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn preserve_block_call(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        lambda_parameters: Option<&[IrParameter]>,
        body: Option<&[IrNode]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<Vec<IrNode>, CallOutcome> {
        let positional_args =
            self.evaluate_values_for_preservation(positional_args, span, diagnostics, context)?;
        let named_args =
            self.evaluate_named_for_preservation(named_args, span, diagnostics, context)?;
        let body = if let Some(nodes) = body {
            let before = diagnostics.len();
            let body = self.evaluate_nodes(nodes, diagnostics, context);
            if diagnostics.len() != before {
                return Err(CallOutcome::Failed);
            }
            Some(body)
        } else {
            None
        };
        Ok(vec![IrNode::FunctionCall {
            name: name.to_string(),
            positional_args,
            named_args,
            lambda_parameters: lambda_parameters.map(ToOwned::to_owned),
            body,
            span: *span,
        }])
    }

    #[allow(clippy::too_many_arguments)]
    fn preserve_inline_call(
        &self,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<&[IrInline]>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<Vec<IrInline>, CallOutcome> {
        let positional_args =
            self.evaluate_values_for_preservation(positional_args, span, diagnostics, context)?;
        let named_args =
            self.evaluate_named_for_preservation(named_args, span, diagnostics, context)?;
        let body = if let Some(inlines) = body {
            let before = diagnostics.len();
            let body = self.evaluate_inlines(inlines, diagnostics, context);
            if diagnostics.len() != before {
                return Err(CallOutcome::Failed);
            }
            Some(body)
        } else {
            None
        };
        Ok(vec![IrInline::DirectiveCall {
            name: name.to_string(),
            positional_args,
            named_args,
            body,
            span: *span,
        }])
    }

    fn materialize_block_value(
        &self,
        value: IrValue,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<Vec<IrNode>, CallOutcome> {
        match value {
            IrValue::Content(nodes) => Ok(nodes),
            IrValue::Collection(values) => {
                let mut nodes = Vec::new();
                for value in values {
                    let materialized = self.materialize_block_value(value, span, diagnostics)?;
                    if let Err(error) = nodes.try_reserve(materialized.len()) {
                        diagnostics.push(iteration_error(
                            format!("collection output cannot be allocated: {error}"),
                            *span,
                        ));
                        return Err(CallOutcome::Failed);
                    }
                    nodes.extend(materialized);
                }
                Ok(nodes)
            }
            IrValue::Pair(pair) => pair_into_content_nodes(pair, diagnostics),
            IrValue::Dictionary(dictionary) => {
                dictionary_into_table(dictionary, diagnostics).map(|table| vec![table])
            }
            IrValue::Component(component) => Ok(vec![IrNode::Component { component }]),
            IrValue::Range(range) => {
                diagnostics.push(iteration_error(
                    "Direct Range materialization is deferred; consume the typed Range through iteration first"
                        .to_string(),
                    range.span,
                ));
                Err(CallOutcome::Failed)
            }
            value => match scalar_to_text(&value, *span, diagnostics) {
                Ok(content) => Ok(vec![IrNode::Paragraph {
                    content: vec![IrInline::Text {
                        content,
                        span: *span,
                    }],
                    span: *span,
                }]),
                Err(outcome) => Err(outcome),
            },
        }
    }

    fn materialize_inline_value(
        &self,
        value: Option<IrValue>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Vec<IrInline> {
        match value {
            Some(IrValue::Content(nodes)) => {
                self.materialize_inline_content(nodes, span, diagnostics)
            }
            Some(IrValue::Collection(values)) => {
                if values.len() != 1 {
                    diagnostics.push(function_error(
                        "A Collection cannot be flattened into inline content unless it has exactly one element"
                            .to_string(),
                        *span,
                    ));
                    Vec::new()
                } else {
                    self.materialize_inline_value(values.into_iter().next(), span, diagnostics)
                }
            }
            Some(IrValue::Component(component)) => {
                diagnostics.push(component_inline_materialization_error(component.span()));
                Vec::new()
            }
            Some(IrValue::Range(range)) => {
                diagnostics.push(iteration_error(
                    "Direct Range materialization is deferred; consume the typed Range through iteration first"
                        .to_string(),
                    range.span,
                ));
                Vec::new()
            }
            Some(value) => match scalar_to_text(&value, *span, diagnostics) {
                Ok(content) => vec![IrInline::Text {
                    content,
                    span: *span,
                }],
                Err(_) => Vec::new(),
            },
            None => Vec::new(),
        }
    }

    /// Materializes only content that has an unambiguous inline shape.
    ///
    /// A paragraph boundary or any other block node must remain observable;
    /// silently concatenating or dropping it would change the document.
    fn materialize_inline_content(
        &self,
        nodes: Vec<IrNode>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Vec<IrInline> {
        let mut nodes = nodes.into_iter();
        let Some(first) = nodes.next() else {
            return Vec::new();
        };
        if nodes.next().is_some() {
            diagnostics.push(function_error(
                "Rich block content cannot be inserted into an inline context unless it is exactly one paragraph".to_string(),
                *span,
            ));
            return Vec::new();
        }
        match first {
            IrNode::Paragraph { content, .. } => content,
            IrNode::TargetSpecificContent { content } => {
                vec![IrInline::TargetSpecificContent { content }]
            }
            _ => {
                diagnostics.push(function_error(
                    "Rich block content cannot be inserted into an inline context unless it is exactly one paragraph".to_string(),
                    *span,
                ));
                Vec::new()
            }
        }
    }

    /// Evaluates a value without entering document-output context.
    fn evaluate_value(
        &self,
        value: &IrValue,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        match value {
            IrValue::Content(nodes) => {
                if let [IrNode::FunctionCall {
                    name,
                    positional_args,
                    named_args,
                    lambda_parameters,
                    body,
                    span,
                }] = nodes.as_slice()
                {
                    return self.evaluate_call_value(
                        name,
                        positional_args,
                        named_args,
                        body.as_deref().map(CallBody::Block),
                        lambda_parameters.as_deref(),
                        span,
                        diagnostics,
                        context,
                    );
                }
                if let [IrNode::ChainedFunctionCall {
                    head, chain, body, ..
                }] = nodes.as_slice()
                {
                    return self.evaluate_chain_value(
                        head,
                        chain,
                        body.as_deref().map(CallBody::Block),
                        diagnostics,
                        context,
                    );
                }
                let before = diagnostics.len();
                let contains_declaration = nodes
                    .iter()
                    .any(|node| matches!(node, IrNode::FunctionDeclaration { .. }));
                let nodes = self.evaluate_nodes(nodes, diagnostics, context);
                if diagnostics.len() == before {
                    if nodes.is_empty() && contains_declaration {
                        CallOutcome::NoValue
                    } else {
                        CallOutcome::Value(IrValue::Content(nodes))
                    }
                } else {
                    CallOutcome::Failed
                }
            }
            IrValue::Callable(callable) => {
                if callable.capture.is_some() {
                    CallOutcome::Value(value.clone())
                } else {
                    let mut callable = callable.clone();
                    callable.capture = Some(Box::new(context.capture_snapshot()));
                    CallOutcome::Value(IrValue::Callable(callable))
                }
            }
            scalar => CallOutcome::Value(scalar.clone()),
        }
    }

    fn evaluate_values(
        &self,
        values: &[IrValue],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<Vec<IrValue>, CallOutcome> {
        let mut evaluated = Vec::new();
        if let Err(error) = evaluated.try_reserve(values.len()) {
            diagnostics.push(iteration_error(
                format!("call arguments cannot be allocated: {error}"),
                *span,
            ));
            return Err(CallOutcome::Failed);
        }
        for value in values {
            match self.evaluate_value(value, diagnostics, context) {
                CallOutcome::Value(value) => evaluated.push(value),
                CallOutcome::Unresolved => {
                    evaluated.push(self.preserve_value_expression(value, diagnostics, context)?)
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(value, span)));
                    return Err(CallOutcome::Failed);
                }
                CallOutcome::Failed => return Err(CallOutcome::Failed),
            }
        }
        Ok(evaluated)
    }

    /// Evaluates arguments while preserving the invocation-time distinction
    /// used by Quarkdown's `RegularArgumentsBinder`. A raw scalar or a
    /// variable/custom-function reference is dynamic; a nested builtin result
    /// such as `.string` is already a static semantic value.
    fn evaluate_invocation_values(
        &self,
        values: &[IrValue],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
        first_origin: Option<ValueOrigin>,
    ) -> Result<Vec<InvocationValue>, CallOutcome> {
        let mut evaluated = Vec::new();
        if let Err(error) = evaluated.try_reserve(values.len()) {
            diagnostics.push(iteration_error(
                format!("call arguments cannot be allocated: {error}"),
                *span,
            ));
            return Err(CallOutcome::Failed);
        }
        for (index, value) in values.iter().enumerate() {
            let origin = if index == 0 {
                first_origin.unwrap_or_else(|| invocation_origin(value, context))
            } else {
                invocation_origin(value, context)
            };
            let evaluated_value = match self.evaluate_value(value, diagnostics, context) {
                CallOutcome::Value(value) => value,
                CallOutcome::Unresolved => {
                    self.preserve_value_expression(value, diagnostics, context)?
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(value, span)));
                    return Err(CallOutcome::Failed);
                }
                CallOutcome::Failed => return Err(CallOutcome::Failed),
            };
            evaluated.push(InvocationValue {
                value: evaluated_value,
                origin,
            });
        }
        Ok(evaluated)
    }

    fn evaluate_invocation_named(
        &self,
        named: &[IrNamedArg],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<Vec<InvocationNamedArg>, CallOutcome> {
        let mut evaluated = Vec::new();
        if let Err(error) = evaluated.try_reserve(named.len()) {
            diagnostics.push(iteration_error(
                format!("named call arguments cannot be allocated: {error}"),
                *span,
            ));
            return Err(CallOutcome::Failed);
        }
        for argument in named {
            let origin = invocation_origin(&argument.value, context);
            let value = match self.evaluate_value(&argument.value, diagnostics, context) {
                CallOutcome::Value(value) => value,
                CallOutcome::Unresolved => {
                    self.preserve_value_expression(&argument.value, diagnostics, context)?
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(&argument.value, span)));
                    return Err(CallOutcome::Failed);
                }
                CallOutcome::Failed => return Err(CallOutcome::Failed),
            };
            evaluated.push(InvocationNamedArg::new(
                IrNamedArg {
                    name: argument.name.clone(),
                    name_span: argument.name_span,
                    value,
                    span: argument.span,
                },
                origin,
            ));
        }
        Ok(evaluated)
    }

    fn evaluate_named(
        &self,
        named: &[IrNamedArg],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<Vec<IrNamedArg>, CallOutcome> {
        let mut evaluated = Vec::new();
        if let Err(error) = evaluated.try_reserve(named.len()) {
            diagnostics.push(iteration_error(
                format!("named call arguments cannot be allocated: {error}"),
                *span,
            ));
            return Err(CallOutcome::Failed);
        }
        for arg in named {
            let value = match self.evaluate_value(&arg.value, diagnostics, context) {
                CallOutcome::Value(value) => value,
                CallOutcome::Unresolved => {
                    self.preserve_value_expression(&arg.value, diagnostics, context)?
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(&arg.value, span)));
                    return Err(CallOutcome::Failed);
                }
                CallOutcome::Failed => return Err(CallOutcome::Failed),
            };
            evaluated.push(IrNamedArg {
                name: arg.name.clone(),
                name_span: arg.name_span,
                value,
                span: arg.span,
            });
        }
        Ok(evaluated)
    }

    fn evaluate_values_for_preservation(
        &self,
        values: &[IrValue],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<Vec<IrValue>, CallOutcome> {
        let mut evaluated = Vec::with_capacity(values.len());
        for value in values {
            match self.evaluate_value(value, diagnostics, context) {
                CallOutcome::Value(value) => {
                    self.validate_preserved_value(&value, diagnostics)?;
                    evaluated.push(value);
                }
                CallOutcome::Unresolved => {
                    evaluated.push(self.preserve_value_expression(value, diagnostics, context)?)
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(value, span)));
                    return Err(CallOutcome::Failed);
                }
                CallOutcome::Failed => return Err(CallOutcome::Failed),
            }
        }
        Ok(evaluated)
    }

    fn evaluate_named_for_preservation(
        &self,
        named: &[IrNamedArg],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> Result<Vec<IrNamedArg>, CallOutcome> {
        let mut evaluated = Vec::with_capacity(named.len());
        for arg in named {
            let value = match self.evaluate_value(&arg.value, diagnostics, context) {
                CallOutcome::Value(value) => {
                    self.validate_preserved_value(&value, diagnostics)?;
                    value
                }
                CallOutcome::Unresolved => {
                    self.preserve_value_expression(&arg.value, diagnostics, context)?
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(&arg.value, span)));
                    return Err(CallOutcome::Failed);
                }
                CallOutcome::Failed => return Err(CallOutcome::Failed),
            };
            evaluated.push(IrNamedArg {
                name: arg.name.clone(),
                name_span: arg.name_span,
                value,
                span: arg.span,
            });
        }
        Ok(evaluated)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeDispatchOwner {
    #[cfg(test)]
    RegularScalar,
    Conditional,
    DocumentState,
    Html,
    Markdown,
    Resource,
    Let,
    Foreach,
    Repeat,
    OptionalityCallback,
    VariableState,
    Center,
    Align,
    Container,
    Landscape,
    Br,
    Whitespace,
    StackedLayout,
    Range,
    Pair,
    Dictionary,
    CollectionAccess,
    CollectionTransform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeOwnerInventory {
    pub(crate) owner: NativeDispatchOwner,
    pub(crate) names: &'static [&'static str],
}

const CONDITIONAL_NATIVE_NAMES: &[&str] = &["if", "ifnot"];
const DOCUMENT_STATE_NATIVE_NAMES: &[&str] = &["docname", "docdescription", "doctype"];
const HTML_NATIVE_NAMES: &[&str] = &["html"];
const MARKDOWN_NATIVE_NAMES: &[&str] = &["markdown"];
const RESOURCE_NATIVE_NAMES: &[&str] = &["read", "json", "include"];
const LET_NATIVE_NAMES: &[&str] = &["let"];
const FOREACH_NATIVE_NAMES: &[&str] = &["foreach"];
const REPEAT_NATIVE_NAMES: &[&str] = &["repeat"];
const OPTIONALITY_CALLBACK_NATIVE_NAMES: &[&str] = &["ifpresent", "takeif"];
const VARIABLE_STATE_NATIVE_NAMES: &[&str] = &["var"];
const CENTER_NATIVE_NAMES: &[&str] = &["center"];
const ALIGN_NATIVE_NAMES: &[&str] = &["align"];
const CONTAINER_NATIVE_NAMES: &[&str] = &["container"];
const LANDSCAPE_NATIVE_NAMES: &[&str] = &["landscape"];
const BR_NATIVE_NAMES: &[&str] = &["br"];
const WHITESPACE_NATIVE_NAMES: &[&str] = &["whitespace"];
const STACKED_LAYOUT_NATIVE_NAMES: &[&str] = &["row", "column", "grid"];
const RANGE_NATIVE_NAMES: &[&str] = &["range"];
const PAIR_NATIVE_NAMES: &[&str] = &["pair"];
const DICTIONARY_NATIVE_NAMES: &[&str] = &["dictionary"];
const COLLECTION_ACCESS_NATIVE_NAMES: &[&str] = &[
    "size",
    "first",
    "second",
    "third",
    "last",
    "getat",
    "sumall",
    "average",
    "distinct",
    "reversed",
    "groupvalues",
];
const COLLECTION_TRANSFORM_NATIVE_NAMES: &[&str] = &["map", "filter", "sorted"];
const DEFERRED_NATIVE_NAMES: &[&str] = &["llmstxt"];

static BESPOKE_NATIVE_OWNERS: &[NativeOwnerInventory] = &[
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Conditional,
        names: CONDITIONAL_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::DocumentState,
        names: DOCUMENT_STATE_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Html,
        names: HTML_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Markdown,
        names: MARKDOWN_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Resource,
        names: RESOURCE_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Let,
        names: LET_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Foreach,
        names: FOREACH_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Repeat,
        names: REPEAT_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::OptionalityCallback,
        names: OPTIONALITY_CALLBACK_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::VariableState,
        names: VARIABLE_STATE_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Center,
        names: CENTER_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Align,
        names: ALIGN_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Container,
        names: CONTAINER_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Landscape,
        names: LANDSCAPE_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Br,
        names: BR_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Whitespace,
        names: WHITESPACE_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::StackedLayout,
        names: STACKED_LAYOUT_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Range,
        names: RANGE_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Pair,
        names: PAIR_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::Dictionary,
        names: DICTIONARY_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::CollectionAccess,
        names: COLLECTION_ACCESS_NATIVE_NAMES,
    },
    NativeOwnerInventory {
        owner: NativeDispatchOwner::CollectionTransform,
        names: COLLECTION_TRANSFORM_NATIVE_NAMES,
    },
];

#[cfg(test)]
pub(crate) fn bespoke_native_owners() -> &'static [NativeOwnerInventory] {
    BESPOKE_NATIVE_OWNERS
}

#[cfg(test)]
pub(crate) fn deferred_native_names() -> &'static [&'static str] {
    DEFERRED_NATIVE_NAMES
}

#[cfg(test)]
pub(crate) fn native_dispatch_owner(name: &str) -> Option<NativeDispatchOwner> {
    let regular = builtins::lookup(name).is_some();
    let bespoke = BESPOKE_NATIVE_OWNERS
        .iter()
        .filter(|inventory| inventory.names.contains(&name))
        .map(|inventory| inventory.owner)
        .collect::<Vec<_>>();
    if regular && bespoke.is_empty() {
        return Some(NativeDispatchOwner::RegularScalar);
    }
    if !regular && bespoke.len() == 1 {
        return bespoke.into_iter().next();
    }
    None
}

fn has_native_owner(name: &str, owner: NativeDispatchOwner) -> bool {
    BESPOKE_NATIVE_OWNERS
        .iter()
        .any(|inventory| inventory.owner == owner && inventory.names.contains(&name))
}

fn is_stacked_layout(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::StackedLayout)
}

fn is_center(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Center)
}

fn is_align(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Align)
}

fn is_container(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Container)
}

fn is_landscape(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Landscape)
}

fn is_br(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Br)
}

fn is_whitespace(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Whitespace)
}

fn is_document_state(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::DocumentState)
}

fn is_html(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Html)
}

fn is_markdown(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Markdown)
}

fn is_resource(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Resource)
}

fn is_deferred(name: &str) -> bool {
    DEFERRED_NATIVE_NAMES.contains(&name)
}

fn bind_whitespace_arguments(
    positional: Vec<WhitespaceArgument>,
    named: Vec<InvocationNamedArg>,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<BoundWhitespaceArguments, CallOutcome> {
    let mut values: [Option<WhitespaceArgument>; 2] = [None, None];
    for (index, argument) in positional.into_iter().enumerate() {
        let Some(slot) = values.get_mut(index) else {
            diagnostics.push(whitespace_argument_error(
                "`.whitespace` accepts at most two positional arguments",
                *span,
            ));
            return Err(CallOutcome::Failed);
        };
        *slot = Some(argument);
    }

    for argument in named {
        let Some(index) = ["width", "height"]
            .iter()
            .position(|parameter| *parameter == argument.arg.name)
        else {
            diagnostics.push(whitespace_argument_error_at(
                format!("Unknown named argument `{}`", argument.arg.name),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        };
        let Some(slot) = values.get_mut(index) else {
            diagnostics.push(whitespace_argument_error_at(
                format!(
                    "Named argument `{}` is outside the `.whitespace` signature",
                    argument.arg.name
                ),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        };
        if slot.is_some() {
            diagnostics.push(whitespace_argument_error_at(
                format!("Argument `{}` was bound more than once", argument.arg.name),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        }
        *slot = Some(WhitespaceArgument {
            value: InvocationValue {
                value: argument.arg.value,
                origin: argument.origin,
            },
            span: argument.arg.span,
        });
    }

    Ok(BoundWhitespaceArguments {
        width: values[0].take(),
        height: values[1].take(),
    })
}

fn convert_whitespace_size(
    argument: &InvocationValue,
) -> Result<Option<IrSize>, value_conversion::ConversionError> {
    if matches!(&argument.value, IrValue::None) {
        return Ok(None);
    }
    match value_conversion::convert_domain_with_origin(
        argument,
        value_conversion::DomainTarget::Size,
    )? {
        value_conversion::DomainValue::Size(value) => Ok(Some(value)),
        _ => Err(value_conversion::ConversionError::UnsupportedValue {
            target: value_conversion::ConversionTarget::Size,
        }),
    }
}

fn zero_whitespace_size() -> IrSize {
    IrSize {
        value: 0.0,
        unit: IrSizeUnit::Px,
    }
}

fn bind_container_arguments(
    positional: Vec<ContainerArgument>,
    named: Vec<InvocationNamedArg>,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<BoundContainerArguments, CallOutcome> {
    let mut values: Vec<Option<ContainerArgument>> = vec![None, None, None];
    for (index, argument) in positional.into_iter().enumerate() {
        let Some(slot) = values.get_mut(index) else {
            diagnostics.push(container_argument_error(
                "`.container` accepts at most three positional arguments",
                *span,
            ));
            return Err(CallOutcome::Failed);
        };
        *slot = Some(argument);
    }

    for argument in named {
        if is_deferred_container_parameter(&argument.arg.name) {
            diagnostics.push(container_argument_error_at(
                format!(
                    "`.container` parameter `{}` is not supported by the bounded container sizing slice",
                    argument.arg.name
                ),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        }
        let Some(index) = ["width", "height", "fullwidth"]
            .iter()
            .position(|parameter| *parameter == argument.arg.name)
        else {
            diagnostics.push(container_argument_error_at(
                format!("Unknown named argument `{}`", argument.arg.name),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        };
        let Some(slot) = values.get_mut(index) else {
            diagnostics.push(container_argument_error_at(
                format!(
                    "Named argument `{}` is outside the bounded container signature",
                    argument.arg.name
                ),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        };
        if slot.is_some() {
            diagnostics.push(container_argument_error_at(
                format!("Argument `{}` was bound more than once", argument.arg.name),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        }
        *slot = Some(ContainerArgument {
            value: InvocationValue {
                value: argument.arg.value,
                origin: argument.origin,
            },
            span: argument.arg.span,
        });
    }

    Ok(BoundContainerArguments {
        width: values[0].take(),
        height: values[1].take(),
        full_width: values[2].take(),
    })
}

fn is_deferred_container_parameter(name: &str) -> bool {
    matches!(
        name,
        "float"
            | "fullspan"
            | "classname"
            | "foreground"
            | "background"
            | "border"
            | "borderwidth"
            | "borderstyle"
            | "alignment"
            | "textalignment"
            | "margin"
            | "padding"
            | "radius"
            | "fontsize"
            | "fontweight"
            | "fontstyle"
            | "fontvariant"
            | "textdecoration"
            | "textcase"
    )
}

fn convert_container_size(
    argument: &InvocationValue,
) -> Result<IrSize, value_conversion::ConversionError> {
    if matches!(&argument.value, IrValue::None) {
        return Err(value_conversion::ConversionError::UnsupportedValue {
            target: value_conversion::ConversionTarget::Size,
        });
    }
    match value_conversion::convert_domain_with_origin(
        argument,
        value_conversion::DomainTarget::Size,
    )? {
        value_conversion::DomainValue::Size(value) => Ok(value),
        _ => Err(value_conversion::ConversionError::UnsupportedValue {
            target: value_conversion::ConversionTarget::Size,
        }),
    }
}

fn convert_container_boolean(
    argument: &InvocationValue,
) -> Result<bool, value_conversion::ConversionError> {
    match value_conversion::convert_scalar_with_origin(argument, ScalarTarget::Boolean)? {
        ScalarValue::Boolean(value) => Ok(value),
        _ => Err(value_conversion::ConversionError::UnsupportedValue {
            target: value_conversion::ConversionTarget::Boolean,
        }),
    }
}

fn container_conversion_error(
    parameter: &str,
    span: SourceSpan,
    error: value_conversion::ConversionError,
) -> Diagnostic {
    let detail = match error {
        value_conversion::ConversionError::InvalidText { .. } => {
            "value is invalid for the typed parameter"
        }
        value_conversion::ConversionError::UnsupportedValue { .. } => {
            "value has the wrong typed domain or origin"
        }
    };
    container_argument_error_at(
        format!("`.container` parameter `{parameter}`: {detail}"),
        span,
    )
}

fn container_argument_error(message: &str, span: SourceSpan) -> Diagnostic {
    container_argument_error_at(message.to_string(), span)
}

fn container_argument_error_at(message: String, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Container arguments are validated before the Markdown body is evaluated.".to_string(),
        ],
    }
}

fn bind_align_argument(
    positional: Vec<AlignArgument>,
    named: Vec<InvocationNamedArg>,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<AlignArgument, CallOutcome> {
    let mut alignment = match positional.as_slice() {
        [] => None,
        [argument] => Some(argument.clone()),
        _ => {
            diagnostics.push(align_argument_error(
                "`.align` accepts exactly one `alignment` argument",
                *span,
            ));
            return Err(CallOutcome::Failed);
        }
    };
    for argument in named {
        if argument.arg.name != "alignment" {
            diagnostics.push(align_argument_error_at(
                format!("Unknown named argument `{}`", argument.arg.name),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        }
        if alignment.is_some() {
            diagnostics.push(align_argument_error_at(
                "Argument `alignment` was bound more than once".to_string(),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        }
        alignment = Some(AlignArgument {
            value: InvocationValue {
                value: argument.arg.value,
                origin: argument.origin,
            },
            span: argument.arg.span,
        });
    }
    alignment.ok_or_else(|| {
        diagnostics.push(align_argument_error(
            "`.align` requires the `alignment` argument",
            *span,
        ));
        CallOutcome::Failed
    })
}

fn convert_align_alignment(
    argument: &InvocationValue,
) -> Result<IrContainerAlignment, value_conversion::ConversionError> {
    match value_conversion::convert_domain_with_origin(
        argument,
        value_conversion::DomainTarget::ClosedEnum(
            value_conversion::ClosedEnumTarget::ContainerAlignment,
        ),
    )? {
        value_conversion::DomainValue::Enum(IrEnumValue::ContainerAlignment(value)) => Ok(value),
        value_conversion::DomainValue::Enum(_) => {
            Err(value_conversion::ConversionError::UnsupportedValue {
                target: value_conversion::ConversionTarget::Enum,
            })
        }
        _ => Err(value_conversion::ConversionError::UnsupportedValue {
            target: value_conversion::ConversionTarget::Enum,
        }),
    }
}

fn bind_stacked_arguments(
    name: &str,
    positional: Vec<StackedArgument>,
    named: Vec<InvocationNamedArg>,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<BoundStackedArguments, CallOutcome> {
    let parameter_names: &[&str] = match name {
        "row" | "column" => &["alignment", "cross", "gap"],
        "grid" => &["columns", "alignment", "cross", "gap", "vgap", "hgap"],
        _ => return Err(CallOutcome::Unresolved),
    };
    let mut values = vec![None; parameter_names.len()];
    for (index, argument) in positional.into_iter().enumerate() {
        let Some(slot) = values.get_mut(index) else {
            diagnostics.push(stacked_argument_error(
                name,
                "positional",
                *span,
                "too many positional arguments",
            ));
            return Err(CallOutcome::Failed);
        };
        *slot = Some(argument);
    }
    for argument in named {
        let Some(index) = parameter_names
            .iter()
            .position(|parameter| *parameter == argument.arg.name)
        else {
            diagnostics.push(stacked_argument_error_at(
                format!("Unknown named argument `{}`", argument.arg.name),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        };
        let Some(slot) = values.get_mut(index) else {
            diagnostics.push(stacked_argument_error(
                name,
                "named",
                argument.arg.name_span,
                "named argument binding is outside the layout signature",
            ));
            return Err(CallOutcome::Failed);
        };
        if slot.is_some() {
            diagnostics.push(stacked_argument_error_at(
                format!("Argument `{}` was bound more than once", argument.arg.name),
                argument.arg.name_span,
            ));
            return Err(CallOutcome::Failed);
        }
        *slot = Some(StackedArgument {
            value: InvocationValue {
                value: argument.arg.value,
                origin: argument.origin,
            },
            span: argument.arg.span,
        });
    }
    Ok(BoundStackedArguments { values })
}

fn convert_stacked_main_axis(
    argument: &InvocationValue,
) -> Result<IrMainAxisAlignment, value_conversion::ConversionError> {
    match value_conversion::convert_domain_with_origin(
        argument,
        value_conversion::DomainTarget::ClosedEnum(
            value_conversion::ClosedEnumTarget::StackedMainAxisAlignment,
        ),
    )? {
        value_conversion::DomainValue::Enum(IrEnumValue::StackedMainAxisAlignment(value)) => {
            Ok(value)
        }
        value_conversion::DomainValue::Enum(_) => {
            Err(value_conversion::ConversionError::UnsupportedValue {
                target: value_conversion::ConversionTarget::Enum,
            })
        }
        _ => Err(value_conversion::ConversionError::UnsupportedValue {
            target: value_conversion::ConversionTarget::Enum,
        }),
    }
}

fn convert_stacked_cross_axis(
    argument: &InvocationValue,
) -> Result<IrCrossAxisAlignment, value_conversion::ConversionError> {
    match value_conversion::convert_domain_with_origin(
        argument,
        value_conversion::DomainTarget::ClosedEnum(
            value_conversion::ClosedEnumTarget::StackedCrossAxisAlignment,
        ),
    )? {
        value_conversion::DomainValue::Enum(IrEnumValue::StackedCrossAxisAlignment(value)) => {
            Ok(value)
        }
        value_conversion::DomainValue::Enum(_) => {
            Err(value_conversion::ConversionError::UnsupportedValue {
                target: value_conversion::ConversionTarget::Enum,
            })
        }
        _ => Err(value_conversion::ConversionError::UnsupportedValue {
            target: value_conversion::ConversionTarget::Enum,
        }),
    }
}

fn convert_stacked_size(
    argument: &InvocationValue,
) -> Result<IrSize, value_conversion::ConversionError> {
    match value_conversion::convert_domain_with_origin(
        argument,
        value_conversion::DomainTarget::Size,
    )? {
        value_conversion::DomainValue::Size(value) => Ok(value),
        _ => Err(value_conversion::ConversionError::UnsupportedValue {
            target: value_conversion::ConversionTarget::Size,
        }),
    }
}

fn convert_optional_stacked_size(
    argument: &InvocationValue,
) -> Result<Option<IrSize>, value_conversion::ConversionError> {
    if matches!(argument.value, IrValue::None) {
        Ok(None)
    } else {
        convert_stacked_size(argument).map(Some)
    }
}

fn stacked_conversion_error(
    name: &str,
    parameter: &str,
    span: SourceSpan,
    error: value_conversion::ConversionError,
) -> Diagnostic {
    stacked_argument_error(
        name,
        parameter,
        span,
        match error {
            value_conversion::ConversionError::InvalidText { .. } => {
                "value is invalid for the typed parameter"
            }
            value_conversion::ConversionError::UnsupportedValue { .. } => {
                "value has the wrong typed domain or origin"
            }
        },
    )
}

fn stacked_argument_error(
    name: &str,
    parameter: &str,
    span: SourceSpan,
    message: &str,
) -> Diagnostic {
    stacked_argument_error_at(
        format!("`.{name}` parameter `{parameter}`: {message}"),
        span,
    )
}

fn stacked_argument_error_at(message: String, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Stacked layout arguments are validated before the Markdown body is evaluated."
                .to_string(),
        ],
    }
}

fn split_dictionary_paragraph(
    content: &[IrInline],
    paragraph_span: SourceSpan,
) -> Option<(String, Vec<IrInline>, String, SourceSpan)> {
    let IrInline::Text {
        content: text,
        span: text_span,
    } = content.first()?
    else {
        return None;
    };
    let colon = text.find(':')?;
    let key = text[..colon].trim().to_string();
    let after = &text[colon + 1..];
    let leading = after.len() - after.trim_start().len();
    let trimmed = after.trim();
    let value_start = text_span
        .start
        .checked_add(colon + 1 + leading)
        .unwrap_or(text_span.end);
    let value_end = value_start
        .checked_add(trimmed.len())
        .unwrap_or(value_start);
    let value_span = SourceSpan::new(text_span.source_id, value_start, value_end);
    if !trimmed.is_empty() {
        return Some((key, Vec::new(), trimmed.to_string(), value_span));
    }
    let mut tail = content.get(1..).unwrap_or_default().to_vec();
    if let Some(IrInline::Text {
        content: tail_text,
        span: tail_span,
    }) = tail.first_mut()
    {
        let leading = tail_text.len() - tail_text.trim_start().len();
        let trailing = tail_text.trim().len();
        if leading > 0 || trailing != tail_text.len() {
            let trimmed = tail_text.trim().to_string();
            let start = tail_span
                .start
                .checked_add(leading)
                .unwrap_or(tail_span.end);
            *tail_text = trimmed;
            *tail_span = SourceSpan::new(
                tail_span.source_id,
                start,
                start.saturating_add(tail_text.len()),
            );
        }
    }
    let tail_span = tail
        .first()
        .map(inline_source_span)
        .unwrap_or(paragraph_span);
    Some((key, tail, String::new(), tail_span))
}

fn plain_dictionary_key(content: &[IrInline]) -> Option<String> {
    let [IrInline::Text { content, .. }] = content else {
        return None;
    };
    let key = content.trim();
    (!key.is_empty() && !key.contains(':')).then(|| key.to_string())
}

fn dictionary_scalar_value(text: &str) -> IrValue {
    let text = text.trim();
    if text.len() >= 2 {
        let bytes = text.as_bytes();
        if (bytes[0] == b'"' && bytes[text.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[text.len() - 1] == b'\'')
        {
            return IrValue::String(text[1..text.len() - 1].to_string());
        }
    }
    match text.to_ascii_lowercase().as_str() {
        "true" | "yes" => IrValue::Boolean(true),
        "false" | "no" => IrValue::Boolean(false),
        _ => text
            .parse::<f64>()
            .map_or_else(|_| IrValue::String(text.to_string()), IrValue::Number),
    }
}

fn dictionary_inline_value(inlines: Vec<IrInline>, span: SourceSpan) -> IrValue {
    match inlines.as_slice() {
        [IrInline::DirectiveCall {
            name,
            positional_args,
            named_args,
            body,
            span,
        }] => IrValue::Content(vec![IrNode::FunctionCall {
            name: name.clone(),
            positional_args: positional_args.clone(),
            named_args: named_args.clone(),
            lambda_parameters: None,
            body: body.as_ref().map(|body| {
                vec![IrNode::Paragraph {
                    content: body.clone(),
                    span: *span,
                }]
            }),
            span: *span,
        }]),
        [IrInline::ChainedDirectiveCall {
            head,
            chain,
            body,
            span,
        }] => IrValue::Content(vec![IrNode::ChainedFunctionCall {
            head: head.clone(),
            chain: chain.clone(),
            body: body.as_ref().map(|body| {
                vec![IrNode::Paragraph {
                    content: body.clone(),
                    span: *span,
                }]
            }),
            span: *span,
        }]),
        _ => IrValue::Content(vec![IrNode::Paragraph {
            content: inlines,
            span,
        }]),
    }
}

fn inline_source_span(inline: &IrInline) -> SourceSpan {
    match inline {
        IrInline::Text { span, .. }
        | IrInline::Whitespace { span, .. }
        | IrInline::Emphasis { span, .. }
        | IrInline::Strong { span, .. }
        | IrInline::Strikethrough { span, .. }
        | IrInline::DirectiveCall { span, .. }
        | IrInline::ChainedDirectiveCall { span, .. }
        | IrInline::Link { span, .. }
        | IrInline::Image { span, .. }
        | IrInline::Code { span, .. }
        | IrInline::SoftBreak { span }
        | IrInline::HardBreak { span }
        | IrInline::RawHtml { span, .. } => *span,
        IrInline::TargetSpecificContent { content } => content.span,
    }
}

fn body_contains_raw_html(nodes: &[IrNode]) -> bool {
    nodes.iter().any(|node| match node {
        IrNode::RawHtml { .. } => true,
        IrNode::Paragraph { content, .. } | IrNode::Heading { content, .. } => {
            content.iter().any(|inline| {
                matches!(inline, IrInline::RawHtml { .. })
                    || match inline {
                        IrInline::Emphasis { content, .. }
                        | IrInline::Strong { content, .. }
                        | IrInline::Strikethrough { content, .. }
                        | IrInline::Link { content, .. }
                        | IrInline::Image { content, .. } => content
                            .iter()
                            .any(|child| matches!(child, IrInline::RawHtml { .. })),
                        _ => false,
                    }
            })
        }
        IrNode::Blockquote { content, .. } => body_contains_raw_html(content),
        IrNode::UnorderedList { items, .. } | IrNode::OrderedList { items, .. } => {
            items.iter().any(|item| body_contains_raw_html(&item.nodes))
        }
        IrNode::Table { header, rows, .. } => header
            .cells
            .iter()
            .chain(rows.iter().flat_map(|row| row.cells.iter()))
            .any(|cell| {
                cell.content
                    .iter()
                    .any(|inline| matches!(inline, IrInline::RawHtml { .. }))
            }),
        _ => false,
    })
}

fn opaque_html_body_string(nodes: &[IrNode]) -> Option<String> {
    let mut output = String::new();
    for node in nodes {
        match node {
            IrNode::RawHtml { source, .. } => output.push_str(source),
            IrNode::Paragraph { content, .. } | IrNode::Heading { content, .. } => {
                for inline in content {
                    append_opaque_html_inline(inline, &mut output)?;
                }
            }
            _ => return None,
        }
    }
    Some(output)
}

fn append_opaque_html_inline(inline: &IrInline, output: &mut String) -> Option<()> {
    match inline {
        IrInline::Text { content, .. } | IrInline::RawHtml { content, .. } => {
            output.push_str(content);
        }
        IrInline::SoftBreak { .. } | IrInline::HardBreak { .. } => output.push('\n'),
        IrInline::Emphasis { .. }
        | IrInline::Strong { .. }
        | IrInline::Strikethrough { .. }
        | IrInline::DirectiveCall { .. }
        | IrInline::ChainedDirectiveCall { .. }
        | IrInline::Link { .. }
        | IrInline::Image { .. }
        | IrInline::Code { .. }
        | IrInline::Whitespace { .. }
        | IrInline::TargetSpecificContent { .. } => return None,
    }
    Some(())
}

/// Returns true for the conditional constructs this evaluator resolves.
fn is_conditional(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Conditional)
}

/// Returns true for the scoped `.let` semantic form.
fn is_let(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Let)
}

fn is_foreach(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Foreach)
}

fn is_repeat(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Repeat)
}

fn is_optionality_callback(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::OptionalityCallback)
}

fn is_range(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Range)
}

fn is_pair(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Pair)
}

fn is_dictionary(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::Dictionary)
}

fn is_collection_access(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::CollectionAccess)
}

fn is_collection_transform(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::CollectionTransform)
}

fn transform_operands(
    name: &str,
    positional_args: &[IrValue],
    named_args: &[IrNamedArg],
    has_body: bool,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(IrValue, Option<IrValue>), CallOutcome> {
    if positional_args.len() > 2 {
        diagnostics.push(iteration_error(
            format!("`.{name}` accepts an iterable and at most one callback"),
            span,
        ));
        return Err(CallOutcome::Failed);
    }
    let callback_names: &[&str] = match name {
        "filter" | "map" | "sorted" => &["by"],
        _ => &[],
    };
    let collection_names = ["from"];
    let mut collection = positional_args.first().cloned();
    let mut callback = positional_args.get(1).cloned();
    for argument in named_args {
        if collection_names.contains(&argument.name.as_str()) {
            if collection.is_some() {
                diagnostics.push(iteration_error_at(
                    format!("`.{name}` received the iterable argument more than once"),
                    argument.name_span,
                ));
                return Err(CallOutcome::Failed);
            }
            collection = Some(argument.value.clone());
        } else if callback_names.contains(&argument.name.as_str()) {
            if callback.is_some() {
                diagnostics.push(iteration_error_at(
                    format!("`.{name}` received the callback argument more than once"),
                    argument.name_span,
                ));
                return Err(CallOutcome::Failed);
            }
            callback = Some(argument.value.clone());
        } else {
            diagnostics.push(iteration_error_at(
                format!("Unknown named argument `{}` for `.{name}`", argument.name),
                argument.name_span,
            ));
            return Err(CallOutcome::Failed);
        }
    }
    let Some(collection) = collection else {
        diagnostics.push(iteration_error(
            format!("`.{name}` requires an iterable argument"),
            span,
        ));
        return Err(CallOutcome::Failed);
    };
    if has_body && callback.is_some() {
        diagnostics.push(iteration_error(
            format!("`.{name}` received both a callback argument and a block body"),
            span,
        ));
        return Err(CallOutcome::Failed);
    }
    Ok((collection, callback))
}

fn optionality_operands(
    name: &str,
    positional_args: &[IrValue],
    named_args: &[IrNamedArg],
    has_body: bool,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(IrValue, Option<IrValue>), CallOutcome> {
    if positional_args.len() > 2 {
        diagnostics.push(function_error(
            format!("`.{name}` accepts a value and one callback"),
            span,
        ));
        return Err(CallOutcome::Failed);
    }

    let callback_name = if name == "ifpresent" {
        "mapping"
    } else {
        "condition"
    };
    let mut value = positional_args.first().cloned();
    let mut callback = positional_args.get(1).cloned();
    for argument in named_args {
        let target = match argument.name.as_str() {
            "value" => &mut value,
            name if name == callback_name => &mut callback,
            _ => {
                diagnostics.push(function_error_at(
                    format!("Unknown named parameter `{}`", argument.name),
                    argument.name_span,
                ));
                return Err(CallOutcome::Failed);
            }
        };
        if target.is_some() {
            diagnostics.push(function_error_at(
                format!("Parameter `{}` was bound more than once", argument.name),
                argument.name_span,
            ));
            return Err(CallOutcome::Failed);
        }
        *target = Some(argument.value.clone());
    }

    let Some(value) = value else {
        diagnostics.push(function_error(
            format!("`.{name}` requires a value argument"),
            span,
        ));
        return Err(CallOutcome::Failed);
    };
    if has_body && callback.is_some() {
        diagnostics.push(function_error(
            format!("`.{name}` received both a callback argument and a block body"),
            span,
        ));
        return Err(CallOutcome::Failed);
    }
    if !has_body && callback.is_none() {
        diagnostics.push(function_error(
            format!("`.{name}` requires a callback lambda"),
            span,
        ));
        return Err(CallOutcome::Failed);
    }
    Ok((value, callback))
}

fn collection_access_operand(
    name: &str,
    named_parameter: &str,
    positional_args: &[InvocationValue],
    named_args: &[InvocationNamedArg],
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<InvocationValue, CallOutcome> {
    if positional_args.len() > 1 {
        diagnostics.push(iteration_error(
            format!(
                "`.{name}` requires exactly one iterable argument (received {})",
                positional_args.len()
            ),
            *span,
        ));
        return Err(CallOutcome::Failed);
    }

    if let Some(argument) = named_args
        .iter()
        .find(|argument| argument.name != named_parameter)
    {
        diagnostics.push(iteration_error_at(
            format!("Unknown named argument `{}` for `.{name}`", argument.name),
            argument.name_span,
        ));
        return Err(CallOutcome::Failed);
    }
    if let Some(argument) = named_args.get(1) {
        diagnostics.push(iteration_error_at(
            format!("`.{name}` received iterable argument more than once"),
            argument.name_span,
        ));
        return Err(CallOutcome::Failed);
    }
    match (positional_args.first(), named_args.first()) {
        (Some(_), Some(argument)) => {
            diagnostics.push(iteration_error_at(
                format!("`.{name}` received iterable argument more than once"),
                argument.name_span,
            ));
            Err(CallOutcome::Failed)
        }
        (Some(value), None) => Ok(value.clone()),
        (None, Some(argument)) => Ok(InvocationValue {
            value: argument.value.clone(),
            origin: argument.origin,
        }),
        (None, None) => {
            diagnostics.push(iteration_error(
                format!("`.{name}` requires exactly one iterable argument"),
                *span,
            ));
            Err(CallOutcome::Failed)
        }
    }
}

fn range_arguments(
    positional_args: &[IrValue],
    named_args: &[IrNamedArg],
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(Option<IrValue>, Option<IrValue>), CallOutcome> {
    if positional_args.len() > 2 {
        diagnostics.push(iteration_error(
            format!(
                "`.range` accepts at most two positional bounds (received {})",
                positional_args.len()
            ),
            *span,
        ));
        return Err(CallOutcome::Failed);
    }

    let mut start = positional_args.first().cloned();
    let mut end = positional_args.get(1).cloned();
    for argument in named_args {
        let slot = match argument.name.as_str() {
            "from" => &mut start,
            "to" => &mut end,
            _ => {
                diagnostics.push(iteration_error_at(
                    format!("Unknown named argument `{}` for `.range`", argument.name),
                    argument.name_span,
                ));
                return Err(CallOutcome::Failed);
            }
        };
        if slot.is_some() {
            diagnostics.push(iteration_error_at(
                format!("`.range` received `{}` more than once", argument.name),
                argument.name_span,
            ));
            return Err(CallOutcome::Failed);
        }
        *slot = Some(argument.value.clone());
    }
    Ok((start, end))
}

fn getat_operands(
    positional_args: &[InvocationValue],
    named_args: &[InvocationNamedArg],
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(InvocationValue, InvocationValue, IrValue), CallOutcome> {
    if positional_args.len() > 2 {
        diagnostics.push(iteration_error(
            format!(
                "`.getat` accepts an iterable and an index (received {} positional arguments)",
                positional_args.len()
            ),
            *span,
        ));
        return Err(CallOutcome::Failed);
    }

    let mut collection = positional_args.first().cloned();
    let mut index = positional_args.get(1).cloned();
    let mut fallback = None;
    for argument in named_args {
        match argument.name.as_str() {
            "from" => {
                if collection.is_some() {
                    diagnostics.push(iteration_error_at(
                        "`.getat` received the iterable argument more than once".to_string(),
                        argument.name_span,
                    ));
                    return Err(CallOutcome::Failed);
                }
                collection = Some(InvocationValue {
                    value: argument.value.clone(),
                    origin: argument.origin,
                });
            }
            "index" => {
                if index.is_some() {
                    diagnostics.push(iteration_error_at(
                        "`.getat` received the index argument more than once".to_string(),
                        argument.name_span,
                    ));
                    return Err(CallOutcome::Failed);
                }
                index = Some(InvocationValue {
                    value: argument.value.clone(),
                    origin: argument.origin,
                });
            }
            "orelse" => {
                if fallback.is_some() {
                    diagnostics.push(iteration_error_at(
                        "`.getat` received the `orelse` argument more than once".to_string(),
                        argument.name_span,
                    ));
                    return Err(CallOutcome::Failed);
                }
                fallback = Some(argument.value.clone());
            }
            _ => {
                diagnostics.push(iteration_error_at(
                    format!("Unknown named argument `{}` for `.getat`", argument.name),
                    argument.name_span,
                ));
                return Err(CallOutcome::Failed);
            }
        }
    }

    let Some(collection) = collection else {
        diagnostics.push(iteration_error(
            "`.getat` requires an iterable argument".to_string(),
            *span,
        ));
        return Err(CallOutcome::Failed);
    };
    let Some(index) = index else {
        diagnostics.push(iteration_error(
            "`.getat` requires an integer index".to_string(),
            *span,
        ));
        return Err(CallOutcome::Failed);
    };
    Ok((collection, index, fallback.unwrap_or(IrValue::None)))
}

fn exact_collection_length(
    length: usize,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<f64, CallOutcome> {
    const MAX_EXACT_F64_INTEGER: u64 = 1 << 53;
    let Ok(length) = u64::try_from(length) else {
        diagnostics.push(iteration_error(
            "Collection length cannot be represented by the evaluator Number type".to_string(),
            *span,
        ));
        return Err(CallOutcome::Failed);
    };
    if length > MAX_EXACT_F64_INTEGER {
        diagnostics.push(iteration_error(
            "Collection length cannot be represented exactly by the evaluator Number type"
                .to_string(),
            *span,
        ));
        return Err(CallOutcome::Failed);
    }
    Ok(length as f64)
}

fn collection_index(
    value: &IrValue,
    length: f64,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Option<usize>, CallOutcome> {
    let IrValue::Number(index) = value else {
        diagnostics.push(iteration_error(
            "`.getat` requires an integer numeric index".to_string(),
            *span,
        ));
        return Err(CallOutcome::Failed);
    };
    if !index.is_finite() || index.fract() != 0.0 {
        diagnostics.push(iteration_error(
            "`.getat` requires a finite integer numeric index".to_string(),
            *span,
        ));
        return Err(CallOutcome::Failed);
    }

    // Quarkdown accepts Int values here, but Kotlin's getOrNull makes zero,
    // negative, and values beyond the finite collection bounds ordinary
    // misses. Check the bounds before converting so an f64 cannot truncate or
    // saturate into a valid Rust index.
    if *index < 1.0 || *index > length {
        return Ok(None);
    }
    let zero_based = (*index - 1.0) as u64;
    let Ok(zero_based) = usize::try_from(zero_based) else {
        diagnostics.push(iteration_error(
            "`.getat` index cannot be represented by this target".to_string(),
            *span,
        ));
        return Err(CallOutcome::Failed);
    };
    Ok(Some(zero_based))
}

/// Applies Quarkdown's `Value.asDouble()` conversion at the evaluator value
/// boundary. Non-numeric values become zero; String values are parsed when
/// possible, while Boolean, None, structured values, and callables stringify
/// to non-numeric values in the upstream implementation and therefore also
/// become zero.
fn collection_value_as_double(value: &IrValue) -> f64 {
    match value {
        IrValue::Number(value) => *value,
        IrValue::String(value) | IrValue::Identifier(value) => {
            value.trim().parse::<f64>().ok().unwrap_or(0.0)
        }
        _ => 0.0,
    }
}

fn collection_sum_all(elements: &[IrValue]) -> f64 {
    elements
        .iter()
        .fold(0.0, |sum, value| sum + collection_value_as_double(value))
}

fn collection_average(
    elements: &[IrValue],
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<f64, CallOutcome> {
    let length = exact_collection_length(elements.len(), span, diagnostics)?;
    if elements.is_empty() {
        return Ok(f64::NAN);
    }
    Ok(collection_sum_all(elements) / length)
}

/// Value equality used by `.distinct` and `.groupvalues`.
///
/// This is deliberately linear and typed. It does not derive an ordering or
/// hash from debug output, and source spans are ignored for semantic values
/// whose upstream wrappers compare their contained values. Content keeps its
/// structural IR equality, which retains the source-backed identity of rich
/// nodes while allowing plain Markdown list text to be represented as String.
fn collection_values_equal(left: &IrValue, right: &IrValue) -> bool {
    match (left, right) {
        (IrValue::String(left), IrValue::String(right))
        | (IrValue::Identifier(left), IrValue::Identifier(right)) => left == right,
        (IrValue::Number(left), IrValue::Number(right)) => {
            (left.is_nan() && right.is_nan()) || left.total_cmp(right) == Ordering::Equal
        }
        (IrValue::Boolean(left), IrValue::Boolean(right)) => left == right,
        (IrValue::None, IrValue::None) => true,
        (IrValue::Range(left), IrValue::Range(right)) => {
            left.start == right.start && left.end == right.end
        }
        (IrValue::Collection(left), IrValue::Collection(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| collection_values_equal(left, right))
        }
        (IrValue::Pair(left), IrValue::Pair(right)) => {
            collection_values_equal(&left.first, &right.first)
                && collection_values_equal(&left.second, &right.second)
        }
        (IrValue::Dictionary(left), IrValue::Dictionary(right)) => {
            left.entries.len() == right.entries.len()
                && left.entries.iter().all(|left_entry| {
                    right.entries.iter().any(|right_entry| {
                        collection_values_equal(&left_entry.first, &right_entry.first)
                            && collection_values_equal(&left_entry.second, &right_entry.second)
                    })
                })
        }
        (IrValue::Content(left), IrValue::Content(right)) => left == right,
        (IrValue::Callable(left), IrValue::Callable(right)) => left == right,
        _ => false,
    }
}

fn distinct_collection_values(
    elements: Vec<IrValue>,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> CallOutcome {
    let mut distinct = Vec::new();
    if let Err(error) = distinct.try_reserve_exact(elements.len()) {
        diagnostics.push(iteration_error(
            format!("distinct collection cannot be allocated: {error}"),
            span,
        ));
        return CallOutcome::Failed;
    }
    for element in elements {
        if !distinct
            .iter()
            .any(|existing| collection_values_equal(existing, &element))
        {
            distinct.push(element);
        }
    }
    CallOutcome::Value(IrValue::Collection(distinct))
}

fn group_collection_values(
    elements: Vec<IrValue>,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> CallOutcome {
    let mut groups: Vec<Vec<IrValue>> = Vec::new();
    if let Err(error) = groups.try_reserve_exact(elements.len()) {
        diagnostics.push(iteration_error(
            format!("grouped collection cannot be allocated: {error}"),
            span,
        ));
        return CallOutcome::Failed;
    }

    for element in elements {
        let group_index = groups.iter().position(|group| {
            group
                .first()
                .is_some_and(|first| collection_values_equal(first, &element))
        });
        match group_index {
            Some(index) => {
                if let Err(error) = groups[index].try_reserve(1) {
                    diagnostics.push(iteration_error(
                        format!("grouped collection cannot be allocated: {error}"),
                        span,
                    ));
                    return CallOutcome::Failed;
                }
                groups[index].push(element);
            }
            None => {
                let mut group = Vec::new();
                if let Err(error) = group.try_reserve_exact(1) {
                    diagnostics.push(iteration_error(
                        format!("grouped collection cannot be allocated: {error}"),
                        span,
                    ));
                    return CallOutcome::Failed;
                }
                group.push(element);
                groups.push(group);
            }
        }
    }

    let mut grouped = Vec::new();
    if let Err(error) = grouped.try_reserve_exact(groups.len()) {
        diagnostics.push(iteration_error(
            format!("grouped collection result cannot be allocated: {error}"),
            span,
        ));
        return CallOutcome::Failed;
    }
    grouped.extend(groups.into_iter().map(IrValue::Collection));
    CallOutcome::Value(IrValue::Collection(grouped))
}

fn validate_iteration_lambda(
    parameters: Option<&[IrParameter]>,
    name: &str,
    allow_destructuring: bool,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> bool {
    if let Some(parameters) = parameters {
        let valid = if allow_destructuring {
            matches!(parameters.len(), 1 | 2)
        } else {
            parameters.len() == 1
        };
        if !valid {
            let parameter_span = parameters
                .get(1)
                .or_else(|| parameters.first())
                .map(|parameter| parameter.span)
                .unwrap_or(*span);
            diagnostics.push(iteration_error_at(
                format!(
                    "`.{name}` requires one explicit parameter{}",
                    if allow_destructuring {
                        " or exactly two parameters for Pair destructuring"
                    } else {
                        ""
                    }
                ),
                parameter_span,
            ));
            return false;
        }
    }
    true
}

fn bind_invocation_arguments(
    parameters: Option<&[IrParameter]>,
    arguments: Vec<IrValue>,
    allow_destructuring: bool,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<BoundLambdaArguments, CallOutcome> {
    let Some(parameters) = parameters else {
        return Ok(BoundLambdaArguments::Implicit(arguments));
    };

    if allow_destructuring && parameters.len() > 1 && arguments.len() == 1 {
        let bindings =
            scoped_parameter_bindings(&arguments[0], parameters, true, span, diagnostics)?;
        return Ok(BoundLambdaArguments::Explicit(
            bindings.into_iter().map(|(_, value)| value).collect(),
        ));
    }

    if arguments.len() > parameters.len() {
        diagnostics.push(function_error(
            format!(
                "Callable received too many arguments (expected at most {}, received {})",
                parameters.len(),
                arguments.len()
            ),
            span,
        ));
        return Err(CallOutcome::Failed);
    }
    let mut bound = arguments;
    for parameter in parameters.iter().skip(bound.len()) {
        if parameter.optional {
            bound.push(IrValue::None);
        } else {
            diagnostics.push(function_error_at(
                format!("Missing required callable argument `{}`", parameter.name),
                parameter.name_span,
            ));
            return Err(CallOutcome::Failed);
        }
    }
    Ok(BoundLambdaArguments::Explicit(bound))
}

fn scoped_parameter_bindings(
    value: &IrValue,
    parameters: &[IrParameter],
    allow_destructuring: bool,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<(String, IrValue)>, CallOutcome> {
    match parameters {
        [parameter] => Ok(vec![(parameter.name.clone(), value.clone())]),
        [first, second] if allow_destructuring => {
            let IrValue::Pair(pair) = value else {
                diagnostics.push(iteration_error(
                    format!(
                        "Cannot destructure `.foreach` item as `{}` and `{}`: expected a Pair",
                        first.name, second.name
                    ),
                    value_source_span(value, &span),
                ));
                return Err(CallOutcome::Failed);
            };
            Ok(vec![
                (first.name.clone(), (*pair.first).clone()),
                (second.name.clone(), (*pair.second).clone()),
            ])
        }
        _ => {
            diagnostics.push(iteration_error(
                "Unsupported scoped lambda parameter pattern".to_string(),
                span,
            ));
            Err(CallOutcome::Failed)
        }
    }
}

fn repeat_count(value: &IrValue) -> Result<i32, String> {
    let IrValue::Number(number) = value else {
        return Err("`.repeat` requires a semantic Number count".to_string());
    };
    if !number.is_finite() {
        return Err("`.repeat` count must be finite".to_string());
    }
    if *number < 0.0 {
        return Err("`.repeat` count must not be negative".to_string());
    }
    if number.fract() != 0.0 {
        return Err("`.repeat` count must be an integer".to_string());
    }
    if *number == 0.0 {
        return Ok(0);
    }
    number
        .to_string()
        .parse::<i64>()
        .ok()
        .and_then(|count| i32::try_from(count).ok())
        .ok_or_else(|| "`.repeat` count is outside the supported integer range".to_string())
}

/// Converts an evaluator Number to the endpoint type used by Quarkdown's
/// `Number.toInt()` call. Kotlin truncates toward zero, maps NaN to zero, and
/// clamps finite or infinite values outside Int's domain to the nearest Int
/// boundary. The explicit comparisons avoid relying on Rust's float-to-int
/// cast behavior as language semantics.
fn number_to_range_endpoint(value: &InvocationValue) -> Result<i32, String> {
    let Ok(ScalarValue::Number(number)) =
        value_conversion::convert_scalar_with_origin(value, ScalarTarget::Number)
    else {
        return Err("`.range` bounds must be numeric".to_string());
    };
    if number.is_nan() {
        return Ok(0);
    }
    if number <= f64::from(i32::MIN) {
        return Ok(i32::MIN);
    }
    if number >= f64::from(i32::MAX) {
        return Ok(i32::MAX);
    }
    Ok(number.trunc() as i32)
}

/// Parses the numeric part of a parser-preserved implicit parameter call.
///
/// The frontend already enforces the token boundary and rejects `.0`/leading
/// zero spellings. This checked conversion keeps oversized decimal indices
/// deterministic instead of allowing an integer conversion panic.
fn implicit_parameter_index(name: &str) -> Option<ImplicitParameterIndex> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes[0] == b'0' || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut index = 0usize;
    for &byte in bytes {
        let digit = usize::from(byte - b'0');
        let Some(next) = index
            .checked_mul(10)
            .and_then(|value| value.checked_add(digit))
        else {
            return Some(ImplicitParameterIndex::Overflow);
        };
        index = next;
    }
    Some(ImplicitParameterIndex::Valid(index))
}

fn implicit_parameter_error(
    name: &str,
    error: ImplicitParameterError,
    span: SourceSpan,
) -> Diagnostic {
    let message = match error {
        ImplicitParameterError::Missing => {
            format!("Implicit lambda parameter `.{name}` is not bound for this invocation")
        }
        ImplicitParameterError::Overflow => {
            format!("Implicit lambda parameter `.{name}` is too large for this evaluator")
        }
    };
    Diagnostic {
        code: "E3003".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Provide the positional argument required by the implicit lambda parameter."
                .to_string(),
        ],
    }
}

fn resource_diagnostic(
    code: &str,
    message: impl Into<String>,
    span: SourceSpan,
    hint: impl Into<String>,
) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        severity: Severity::Error,
        message: message.into(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![hint.into()],
    }
}

fn resource_access_diagnostic(
    builtin: &str,
    error: ResourceAccessError,
    span: SourceSpan,
) -> Diagnostic {
    match error {
        ResourceAccessError::UnsupportedReference { reference } => resource_diagnostic(
            "E8001",
            format!("`.{builtin}` does not support non-local resource reference `{reference}`"),
            span,
            "Only source-relative paths inside the supplied VirtualProject are available; network fetching is disabled.",
        ),
        ResourceAccessError::UnknownSource { source_id } => resource_diagnostic(
            "E9001",
            format!("`.{builtin}` cannot resolve the current source identity {source_id:?}"),
            span,
            "The host must provide the calling source through the VirtualProject SourceStore.",
        ),
        ResourceAccessError::Boundary { message } => resource_diagnostic(
            "E8001",
            format!("`.{builtin}` resource path is outside the project boundary: {message}"),
            span,
            "Use a source-relative path that remains inside the supplied VirtualProject.",
        ),
        ResourceAccessError::NotFound { path } => resource_diagnostic(
            "E3001",
            format!("`.{builtin}` resource not found: `{path}`"),
            span,
            "Add the logical resource to the VirtualProject supplied by the host.",
        ),
        ResourceAccessError::InvalidUtf8 { path, message } => resource_diagnostic(
            "E3001",
            format!("`.{builtin}` resource `{path}` is not valid UTF-8: {message}"),
            span,
            "Text resource builtins require valid UTF-8 and do not perform lossy decoding.",
        ),
    }
}

fn resource_context<'a>(
    context: &'a EvaluationContext<'_>,
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<(&'a dyn ResourceProvider, SourceId)> {
    let Some(provider) = context.resources else {
        diagnostics.push(resource_diagnostic(
            "E8001",
            "Resource builtin requires a host-supplied VirtualProject".to_string(),
            *span,
            "Compile through the project API so logical resources are supplied explicitly.",
        ));
        return None;
    };
    let Some(source_id) = context.current_source else {
        diagnostics.push(resource_diagnostic(
            "E9001",
            "Resource builtin has no current source identity".to_string(),
            *span,
            "The evaluator must retain the logical source identity of the current document.",
        ));
        return None;
    };
    Some((provider, source_id))
}

fn resource_path_argument(
    builtin: &str,
    positional_args: &[IrValue],
    named_args: &[IrNamedArg],
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<String> {
    if positional_args.len() > 1 {
        diagnostics.push(resource_diagnostic(
            "E3003",
            format!("`.{builtin}` accepts exactly one resource path"),
            *span,
            "Pass one source-relative logical resource path.",
        ));
        return None;
    }
    let mut named_path = None;
    for argument in named_args {
        if argument.name == "path" {
            if named_path.is_some() {
                diagnostics.push(resource_diagnostic(
                    "E3003",
                    format!("`.{builtin}` received `path` more than once"),
                    argument.name_span,
                    "Pass one resource path.",
                ));
                return None;
            }
            named_path = Some(&argument.value);
        } else if !matches!(
            (builtin, argument.name.as_str()),
            ("read", "lines") | ("include", "sandbox")
        ) {
            diagnostics.push(resource_diagnostic(
                "E3003",
                format!(
                    "`.{builtin}` does not support named argument `{}`",
                    argument.name
                ),
                argument.name_span,
                "Use one path argument and only the builtin's documented optional arguments.",
            ));
            return None;
        }
    }
    if positional_args.len() == 1 && named_path.is_some() {
        diagnostics.push(resource_diagnostic(
            "E3003",
            format!("`.{builtin}` received `path` more than once"),
            *span,
            "Use either the positional path or `path`.",
        ));
        return None;
    }
    let Some(value) = positional_args.first().or(named_path) else {
        diagnostics.push(resource_diagnostic(
            "E3003",
            format!("`.{builtin}` requires a resource path"),
            *span,
            "Pass a source-relative logical resource path.",
        ));
        return None;
    };
    let Some(path) = builtins::adapt_string_argument(value) else {
        diagnostics.push(resource_diagnostic(
            "E3003",
            format!("`.{builtin}` resource path must adapt to String"),
            value_source_span(value, span),
            "Use a scalar or plain-text path value.",
        ));
        return None;
    };
    Some(path)
}

fn resource_lines_argument(
    named_args: &[IrNamedArg],
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Option<IrRange>, ()> {
    let mut lines = None;
    for argument in named_args {
        if argument.name != "lines" {
            continue;
        }
        if lines.is_some() {
            diagnostics.push(resource_diagnostic(
                "E3003",
                "`.read` received `lines` more than once".to_string(),
                argument.name_span,
                "Pass one inclusive line range.",
            ));
            return Err(());
        }
        let IrValue::Range(range) = &argument.value else {
            diagnostics.push(resource_diagnostic(
                "E3003",
                "`.read` named argument `lines` must be a typed Range".to_string(),
                argument.span,
                "Use a one-based inclusive range such as `1..3`.",
            ));
            return Err(());
        };
        lines = Some(range.clone());
    }
    let _ = span;
    Ok(lines)
}

#[derive(Debug, Clone, Copy)]
enum IncludeSandbox {
    Share,
    Scope,
    Subdocument,
}

fn include_sandbox_argument(
    named_args: &[IrNamedArg],
    span: &SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<IncludeSandbox, ()> {
    let mut sandbox = None;
    for argument in named_args {
        if argument.name != "sandbox" {
            continue;
        }
        if sandbox.is_some() {
            diagnostics.push(resource_diagnostic(
                "E3003",
                "`.include` received `sandbox` more than once".to_string(),
                argument.name_span,
                "Pass one sandbox mode: share, scope, or subdocument.",
            ));
            return Err(());
        }
        let Some(value) = builtins::adapt_string_argument(&argument.value) else {
            diagnostics.push(resource_diagnostic(
                "E3003",
                "`.include` `sandbox` must be a String".to_string(),
                argument.span,
                "Use `share`, `scope`, or `subdocument`.",
            ));
            return Err(());
        };
        sandbox = Some(match value.to_ascii_lowercase().as_str() {
            "share" => IncludeSandbox::Share,
            "scope" => IncludeSandbox::Scope,
            "subdocument" => IncludeSandbox::Subdocument,
            _ => {
                diagnostics.push(resource_diagnostic(
                    "E3003",
                    format!("unsupported `.include` sandbox `{value}`"),
                    argument.span,
                    "Use `share`, `scope`, or `subdocument`.",
                ));
                return Err(());
            }
        });
    }
    let _ = span;
    Ok(sandbox.unwrap_or(IncludeSandbox::Share))
}

fn normalize_line_separators(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == '\r' {
            if chars.as_str().starts_with('\n') {
                let _ = chars.next();
            }
            normalized.push('\n');
        } else {
            normalized.push(character);
        }
    }
    normalized
}

fn select_lines(text: &str, range: IrRange) -> Result<String, String> {
    let start = range.start.unwrap_or(1);
    let normalized = normalize_line_separators(text);
    let lines = normalized.lines().collect::<Vec<_>>();
    let end = range.end.unwrap_or(lines.len() as i32);
    if start < 1 || end < start || end as usize > lines.len() {
        return Err(format!(
            "range {start}..{end} is outside 1..{}",
            lines.len()
        ));
    }
    Ok(lines[(start as usize - 1)..end as usize].join("\n"))
}

fn json_value_to_ir(value: &serde_json::Value, span: SourceSpan) -> Result<IrValue, String> {
    match value {
        serde_json::Value::Null => Ok(IrValue::None),
        serde_json::Value::Bool(value) => Ok(IrValue::Boolean(*value)),
        serde_json::Value::String(value) => Ok(IrValue::String(value.clone())),
        serde_json::Value::Number(value) => json_number_to_ir(value),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| json_value_to_ir(value, span))
            .collect::<Result<Vec<_>, _>>()
            .map(IrValue::Collection),
        serde_json::Value::Object(entries) => entries
            .iter()
            .map(|(key, value)| {
                Ok(IrPair {
                    first: Box::new(IrValue::String(key.clone())),
                    second: Box::new(json_value_to_ir(value, span)?),
                    span,
                })
            })
            .collect::<Result<Vec<_>, String>>()
            .map(|entries| IrValue::Dictionary(IrDictionary { entries, span })),
    }
}

fn json_number_to_ir(value: &serde_json::Number) -> Result<IrValue, String> {
    const MAX_EXACT_F64_INTEGER: u64 = 9_007_199_254_740_991;
    if let Some(value) = value.as_i64() {
        if value.unsigned_abs() > MAX_EXACT_F64_INTEGER {
            return Err(format!(
                "integer {value} cannot be represented exactly by evaluator Number"
            ));
        }
        return Ok(IrValue::Number(value as f64));
    }
    if let Some(value) = value.as_u64() {
        if value > MAX_EXACT_F64_INTEGER {
            return Err(format!(
                "integer {value} cannot be represented exactly by evaluator Number"
            ));
        }
        return Ok(IrValue::Number(value as f64));
    }
    let value = value
        .as_f64()
        .ok_or_else(|| "JSON number cannot be represented by evaluator Number".to_string())?;
    if !value.is_finite() {
        return Err("JSON number is not finite".to_string());
    }
    Ok(IrValue::Number(value))
}

fn source_mode_for_resource_path(path: &str) -> Mode {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let is_markdown = file_name
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("md"));
    if is_markdown {
        Mode::Markdown
    } else {
        Mode::Quarkdown
    }
}

fn chain_evaluation_error(message: String, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec!["The evaluator did not fabricate a value for the failed call.".to_string()],
    }
}

fn function_error(message: String, span: SourceSpan) -> Diagnostic {
    function_error_at(message, span)
}

fn function_error_at(message: String, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3003".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Function declarations and calls must satisfy the supported required-parameter contract."
                .to_string(),
        ],
    }
}

fn document_state_call_error(message: String, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3003".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Document-state builtins read without arguments and write one validated String argument."
                .to_string(),
        ],
    }
}

fn document_state_conversion_error(message: String, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Document-state mutation is committed only after argument conversion and validation succeed."
                .to_string(),
        ],
    }
}

fn html_argument_error(message: &str, span: SourceSpan) -> Diagnostic {
    html_argument_error_at(message.to_string(), span)
}

fn html_argument_error_at(message: String, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3003".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec!["`.html` accepts exactly one regular `content` String argument.".to_string()],
    }
}

fn native_content_denied(span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3004".to_string(),
        severity: Severity::Error,
        message: "NativeContent capability is required for `.html`".to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Grant the NativeContent capability for this compilation to enable `.html`."
                .to_string(),
        ],
    }
}

fn unsupported_raw_html(span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E8001".to_string(),
        severity: Severity::Error,
        message: "Raw HTML is unsupported outside an owning target-specific function argument"
            .to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Use Quarkdown `.html` for target-specific HTML content; ordinary mixed raw HTML remains unsupported."
                .to_string(),
        ],
    }
}

fn let_error(message: String, span: SourceSpan) -> Diagnostic {
    let mut diagnostic = function_error_at(message, span);
    diagnostic.hints =
        vec!["`.let` requires one value argument and a block lambda body.".to_string()];
    diagnostic
}

fn let_error_at(message: String, span: SourceSpan) -> Diagnostic {
    let mut diagnostic = let_error(message, span);
    diagnostic.primary = Some(span);
    diagnostic
}

fn value_source_span(value: &IrValue, fallback: &SourceSpan) -> SourceSpan {
    match value {
        IrValue::Pair(pair) => pair.span,
        IrValue::Dictionary(dictionary) => dictionary.span,
        IrValue::Range(range) => range.span,
        IrValue::Callable(callable) => callable.span,
        IrValue::Component(component) => component.span(),
        IrValue::Content(nodes) => match nodes.as_slice() {
            [IrNode::FunctionCall { span, .. }] | [IrNode::ChainedFunctionCall { span, .. }] => {
                *span
            }
            [IrNode::FunctionDeclaration { span, .. }] => *span,
            _ => *fallback,
        },
        _ => *fallback,
    }
}

fn no_value_required(span: SourceSpan) -> Diagnostic {
    chain_evaluation_error(
        "Call produced no value where a value is required for semantic composition".to_string(),
        span,
    )
}

fn iteration_error(message: String, span: SourceSpan) -> Diagnostic {
    iteration_error_at(message, span)
}

fn iteration_error_at(message: String, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Iteration values remain typed; unsupported or invalid iteration is not fabricated as text."
                .to_string(),
        ],
    }
}

fn materialized_elements_limit_error(requested: u64, limit: usize, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3005".to_string(),
        severity: Severity::Error,
        message: format!(
            "materialized element limit exceeded: requested {requested}, maximum is {limit}"
        ),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Reduce the size of this range or iterable, or configure a higher evaluator materialization limit."
                .to_string(),
        ],
    }
}

fn evaluation_depth_limit_error(limit: usize, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3005".to_string(),
        severity: Severity::Error,
        message: format!(
            "evaluation depth limit exceeded: maximum is {limit} active evaluator frame(s)"
        ),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Reduce recursive or nested function/callback evaluation, or configure a higher evaluator depth limit."
                .to_string(),
        ],
    }
}

fn stacked_inline_materialization_error(span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message: "Stacked layout is block-only".to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "Use `.row`, `.column`, or `.grid` as a block call with a Markdown body.".to_string(),
        ],
    }
}

fn component_inline_materialization_error(span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message: "Semantic component is block-only".to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec!["Use the component as a block call with a Markdown body.".to_string()],
    }
}

fn center_argument_error(message: &str, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3003".to_string(),
        severity: Severity::Error,
        message: message.to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "`.center` accepts exactly one required Markdown block body and no arguments."
                .to_string(),
        ],
    }
}

fn center_inline_materialization_error(span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message: "`.center` is block-only".to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec!["Use `.center` as a block call with a Markdown body.".to_string()],
    }
}

fn align_argument_error(message: &str, span: SourceSpan) -> Diagnostic {
    align_argument_error_at(message.to_string(), span)
}

fn align_argument_error_at(message: String, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3003".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "`.align` accepts one required alignment argument and one Markdown block body."
                .to_string(),
        ],
    }
}

fn align_conversion_error(
    span: SourceSpan,
    error: value_conversion::ConversionError,
) -> Diagnostic {
    align_argument_error(
        match error {
            value_conversion::ConversionError::InvalidText { .. } => {
                "`.align` alignment value is invalid"
            }
            value_conversion::ConversionError::UnsupportedValue { .. } => {
                "`.align` alignment value has the wrong typed domain or origin"
            }
        },
        span,
    )
}

fn align_inline_materialization_error(span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message: "`.align` is block-only".to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec!["Use `.align` as a block call with a Markdown body.".to_string()],
    }
}

fn container_inline_materialization_error(span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message: "`.container` is block-only".to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec!["Use `.container` as a block call with an optional Markdown body.".to_string()],
    }
}

fn landscape_argument_error(message: &str, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3003".to_string(),
        severity: Severity::Error,
        message: message.to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "`.landscape` accepts exactly one required Markdown block body and no arguments."
                .to_string(),
        ],
    }
}

fn landscape_inline_materialization_error(span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message: "`.landscape` is block-only".to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec!["Use `.landscape` as a block call with a Markdown body.".to_string()],
    }
}

fn br_argument_error(message: &str, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3003".to_string(),
        severity: Severity::Error,
        message: message.to_string(),
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec!["`.br` accepts no arguments and no body.".to_string()],
    }
}

fn whitespace_argument_error(message: &str, span: SourceSpan) -> Diagnostic {
    whitespace_argument_error_at(message.to_string(), span)
}

fn whitespace_argument_error_at(message: String, span: SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3003".to_string(),
        severity: Severity::Error,
        message,
        primary: Some(span),
        secondary: Vec::new(),
        hints: vec![
            "`.whitespace` accepts optional `width` and `height` Size arguments and no body."
                .to_string(),
        ],
    }
}

fn whitespace_conversion_error(
    parameter: &str,
    span: SourceSpan,
    error: value_conversion::ConversionError,
) -> Diagnostic {
    let detail = match error {
        value_conversion::ConversionError::InvalidText { .. } => {
            "value is invalid for the existing Size adapter"
        }
        value_conversion::ConversionError::UnsupportedValue { .. } => {
            "value has the wrong typed domain or origin"
        }
    };
    whitespace_argument_error_at(
        format!("`.whitespace` parameter `{parameter}`: {detail}"),
        span,
    )
}

/// Resolves a value to a boolean, handling variable references.
fn resolve_boolean_value(value: &InvocationValue) -> Option<bool> {
    match value_conversion::convert_scalar_with_origin(value, ScalarTarget::Boolean) {
        Ok(ScalarValue::Boolean(value)) => Some(value),
        Ok(_) | Err(_) => None,
    }
}

/// Maps a scalar value to its boolean meaning (without variable resolution).
/// Supports the Quarkdown boolean literals `true`/`yes` and `false`/`no`,
/// case-insensitive (Quarkdown "Boolean" documentation, badged `v2.5.0`).
fn scalar_boolean_value(value: &IrValue) -> Option<bool> {
    match value {
        IrValue::Boolean(value) => Some(*value),
        _ => None,
    }
}

/// Classifies the value expression at the Quarkdown invocation boundary.
///
/// Raw scalar arguments and references to variables or user functions enter
/// the upstream DynamicValue binder path. A nested builtin such as
/// `.string`, a typed range, or a resource result is already a materialized
/// semantic value and must not be reinterpreted by unrelated target types.
fn invocation_origin(value: &IrValue, context: &EvaluationContext<'_>) -> ValueOrigin {
    match value {
        IrValue::String(_) | IrValue::Identifier(_) => ValueOrigin::Dynamic,
        IrValue::Content(nodes) => match nodes.as_slice() {
            [IrNode::FunctionCall { name, .. }]
            | [IrNode::ChainedFunctionCall {
                head: IrCallSegment { name, .. },
                ..
            }] if context.contains(name) || context.get_function(name).is_some() => {
                ValueOrigin::Dynamic
            }
            _ => ValueOrigin::Static,
        },
        _ => ValueOrigin::Static,
    }
}

fn call_result_origin(name: &str, context: &EvaluationContext<'_>) -> ValueOrigin {
    if context.contains(name) || context.get_function(name).is_some() {
        ValueOrigin::Dynamic
    } else {
        ValueOrigin::Static
    }
}

/// Decides whether a conditional's content is taken.
fn take_branch(name: &str, condition: bool) -> bool {
    if name == "if" {
        condition
    } else {
        !condition
    }
}

/// Returns true for `.var` declarations.
fn is_var_declaration(name: &str) -> bool {
    has_native_owner(name, NativeDispatchOwner::VariableState)
}

/// Returns true if a call is a variable reference (parameterless call to a known variable).
fn is_variable_reference_call(
    name: &str,
    positional_args: &[IrValue],
    named_args: &[IrNamedArg],
    body: Option<CallBody<'_>>,
    context: &EvaluationContext<'_>,
) -> bool {
    // Variable reference: parameterless call (no positional args, no named args, no body)
    // to a name that exists in the variable environment.
    positional_args.is_empty() && named_args.is_empty() && body.is_none() && context.contains(name)
}

/// Returns true if a call is a variable reassignment (`.name {value}` where `name` is a known variable).
fn is_variable_reassignment_call(
    name: &str,
    positional_args: &[IrValue],
    named_args: &[IrNamedArg],
    body: Option<CallBody<'_>>,
    context: &EvaluationContext<'_>,
) -> bool {
    // Variable reassignment: call to a known variable name with exactly one
    // positional argument (the new value), no named args, no body.
    context.contains(name) && positional_args.len() == 1 && named_args.is_empty() && body.is_none()
}

/// Builds the `E3002` diagnostic for an invalid variable declaration.
fn invalid_var_declaration(span: &SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3002".to_string(),
        severity: Severity::Error,
        message: "`.var` declaration requires a name and a value (body, second positional argument, or named `value`/`body` argument)".to_string(),
        primary: Some(*span),
        secondary: Vec::new(),
        hints: vec![
            "Use `.var {name} {value}` or `.var {name}\n    content` for block variables.".to_string(),
        ],
    }
}

/// Builds the `E3002` diagnostic for an invalid variable name.
fn invalid_var_name(name: &str, span: &SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3002".to_string(),
        severity: Severity::Error,
        message: format!("Invalid variable name `{name}`: must match `[A-Za-z_][A-Za-z0-9_-]*`"),
        primary: Some(*span),
        secondary: Vec::new(),
        hints: vec!["Variable names must start with a letter or underscore, followed by letters, digits, underscores, or hyphens.".to_string()],
    }
}

/// Renders a scalar argument as plain text.
///
/// Range and Collection are semantic values, not scalar text. Reaching this
/// helper with either variant is an explicit materialization failure rather
/// than an empty-string fallback.
fn scalar_to_text(
    value: &IrValue,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<String, CallOutcome> {
    match value {
        IrValue::String(text) => Ok(text.clone()),
        IrValue::Number(number) => Ok(scalar_number_to_text(*number)),
        IrValue::Boolean(boolean) => Ok(boolean.to_string()),
        IrValue::Identifier(name) => Ok(name.clone()),
        IrValue::None => Ok("None".to_string()),
        IrValue::Content(_) => {
            diagnostics.push(iteration_error(
                "Rich content cannot be rendered as scalar text".to_string(),
                span,
            ));
            Err(CallOutcome::Failed)
        }
        IrValue::Range(range) => {
            diagnostics.push(iteration_error(
                "Direct Range materialization is deferred; consume the typed Range through iteration first"
                    .to_string(),
                range.span,
            ));
            Err(CallOutcome::Failed)
        }
        IrValue::Collection(_) | IrValue::Pair(_) | IrValue::Dictionary(_) => {
            diagnostics.push(iteration_error(
                "Collection, Pair, or Dictionary cannot be rendered as scalar text".to_string(),
                span,
            ));
            Err(CallOutcome::Failed)
        }
        IrValue::Size(_) | IrValue::Color(_) | IrValue::Enum(_) => {
            diagnostics.push(iteration_error(
                "Domain values cannot be rendered as scalar text without a domain consumer"
                    .to_string(),
                span,
            ));
            Err(CallOutcome::Failed)
        }
        IrValue::Component(component) => {
            diagnostics.push(iteration_error(
                "A semantic component cannot be rendered as scalar text".to_string(),
                component.span(),
            ));
            Err(CallOutcome::Failed)
        }
        IrValue::Callable(_) => {
            diagnostics.push(iteration_error(
                "A callable cannot be rendered as scalar text".to_string(),
                span,
            ));
            Err(CallOutcome::Failed)
        }
    }
}

/// Keeps the shortest decimal representation of numeric builtin results that
/// crossed the upstream `Float` boundary, while preserving f64-only values
/// originating elsewhere in the IR.
fn scalar_number_to_text(number: f64) -> String {
    value_conversion::number_to_text(number)
}

/// Builds the `E3001` diagnostic for an unresolvable condition.
fn unresolvable_condition(name: &str, span: &SourceSpan) -> Diagnostic {
    Diagnostic {
        code: "E3001".to_string(),
        severity: Severity::Error,
        message: format!(
            "`{name}` requires a boolean-compatible condition (literals `true`, `false`, `yes`, `no`, or variable reference `.name`) as its `condition` argument"
        ),
        primary: Some(*span),
        secondary: Vec::new(),
        hints: vec!["Condition must be a boolean literal or a variable reference that resolves to a boolean.".to_string()],
    }
}

impl Evaluator {
    fn handle_function_declaration(
        &self,
        name: &IrValue,
        parameters: &[IrParameter],
        body: &[IrNode],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) {
        let function_name = match name {
            IrValue::Identifier(name) | IrValue::String(name) => name,
            _ => {
                diagnostics.push(function_error(
                    "Function name must be a normal identifier".to_string(),
                    *span,
                ));
                return;
            }
        };
        if !is_valid_normal_call_name(function_name) {
            diagnostics.push(function_error(
                format!("Invalid function name `{function_name}`"),
                *span,
            ));
            return;
        }
        if body.is_empty() {
            diagnostics.push(function_error(
                "Function declaration requires a non-empty body".to_string(),
                *span,
            ));
            return;
        }
        let mut seen = BTreeMap::new();
        for parameter in parameters {
            if seen
                .insert(parameter.name.clone(), parameter.span)
                .is_some()
            {
                diagnostics.push(function_error_at(
                    format!("Duplicate function parameter `{}`", parameter.name),
                    parameter.span,
                ));
                return;
            }
        }
        let lambda_parameters = if parameters.is_empty() {
            LambdaParameters::Implicit
        } else {
            LambdaParameters::Explicit(parameters.to_vec())
        };
        let capture = Some(Box::new(context.capture_snapshot()));
        context.set_function_binding(
            function_name.clone(),
            lambda_parameters,
            body.to_vec(),
            *span,
            capture,
        );
    }

    // Variable handling methods

    /// Handles a `.var` declaration in value context.
    #[allow(clippy::too_many_arguments)]
    fn handle_var_declaration(
        &self,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        body: Option<CallBody<'_>>,
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        // Check for malformed declaration: must have a name (first positional arg)
        let var_name = match positional_args.first() {
            Some(IrValue::Identifier(name)) => name.clone(),
            Some(IrValue::String(name)) => name.clone(),
            _ => {
                diagnostics.push(invalid_var_declaration(span));
                return CallOutcome::Failed;
            }
        };
        // Validate variable name
        if !is_valid_normal_call_name(&var_name) {
            diagnostics.push(invalid_var_name(&var_name, span));
            return CallOutcome::Failed;
        }

        // Determine the value: body > named "body" > second positional > named "value" > empty
        if let Some(body) = body {
            let outcome = match body {
                CallBody::Block(nodes) => {
                    self.evaluate_callable_body_value(nodes, diagnostics, context)
                }
                CallBody::Inline(inlines) => {
                    self.evaluate_call_body(CallBody::Inline(inlines), span, diagnostics, context)
                }
            };
            match outcome {
                CallOutcome::Value(value) => {
                    context.set_value(var_name, value);
                    return CallOutcome::NoValue;
                }
                CallOutcome::Failed => return CallOutcome::Failed,
                CallOutcome::NoValue | CallOutcome::Unresolved => {
                    return CallOutcome::Failed;
                }
            }
        }

        // Check named "body" argument
        if let Some(arg) = named_args.iter().find(|arg| arg.name == "body") {
            let value = &arg.value;
            match self.evaluate_value(value, diagnostics, context) {
                CallOutcome::Value(value) => {
                    context.set_value(var_name, value);
                    return CallOutcome::NoValue;
                }
                CallOutcome::Unresolved => {
                    match self.preserve_value_expression(value, diagnostics, context) {
                        Ok(value) => {
                            context.set_value(var_name, value);
                            return CallOutcome::NoValue;
                        }
                        Err(outcome) => return outcome,
                    }
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(value, span)));
                    return CallOutcome::Failed;
                }
                CallOutcome::Failed => return CallOutcome::Failed,
            }
        }

        // Check named "value" argument
        if let Some(arg) = named_args.iter().find(|arg| arg.name == "value") {
            let value = &arg.value;
            match self.evaluate_value(value, diagnostics, context) {
                CallOutcome::Value(value) => {
                    context.set_value(var_name, value);
                    return CallOutcome::NoValue;
                }
                CallOutcome::Unresolved => {
                    match self.preserve_value_expression(value, diagnostics, context) {
                        Ok(value) => {
                            context.set_value(var_name, value);
                            return CallOutcome::NoValue;
                        }
                        Err(outcome) => return outcome,
                    }
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(value, span)));
                    return CallOutcome::Failed;
                }
                CallOutcome::Failed => return CallOutcome::Failed,
            }
        }

        // Fall back to second positional argument
        if let Some(value) = positional_args.get(1) {
            match self.evaluate_value(value, diagnostics, context) {
                CallOutcome::Value(value) => {
                    context.set_value(var_name, value);
                    return CallOutcome::NoValue;
                }
                CallOutcome::Unresolved => {
                    match self.preserve_value_expression(value, diagnostics, context) {
                        Ok(value) => {
                            context.set_value(var_name, value);
                            return CallOutcome::NoValue;
                        }
                        Err(outcome) => return outcome,
                    }
                }
                CallOutcome::NoValue => {
                    diagnostics.push(no_value_required(value_source_span(value, span)));
                    return CallOutcome::Failed;
                }
                CallOutcome::Failed => return CallOutcome::Failed,
            }
        }

        // No value provided - invalid declaration
        diagnostics.push(invalid_var_declaration(span));
        CallOutcome::Failed
    }

    /// Handles a variable reassignment in value context.
    fn handle_variable_reassignment_value(
        &self,
        name: &str,
        positional_args: &[IrValue],
        span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        let value = &positional_args[0];
        match self.evaluate_value(value, diagnostics, context) {
            CallOutcome::Value(value) => {
                context.set_value(name.to_string(), value);
                CallOutcome::NoValue
            }
            CallOutcome::Unresolved => {
                match self.preserve_value_expression(value, diagnostics, context) {
                    Ok(value) => {
                        context.set_value(name.to_string(), value);
                        CallOutcome::NoValue
                    }
                    Err(outcome) => outcome,
                }
            }
            CallOutcome::NoValue => {
                diagnostics.push(no_value_required(value_source_span(value, span)));
                CallOutcome::Failed
            }
            CallOutcome::Failed => CallOutcome::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scribium_ir::{
        IrComponent, IrCrossAxisAlignment, IrListItem, IrMainAxisAlignment, IrSize, IrSizeUnit,
        IrStackedComponent, IrStackedLayout,
    };
    use scribium_source::SourceId;

    fn span(start: usize, end: usize) -> SourceSpan {
        SourceSpan::new(SourceId(1), start, end)
    }

    fn named_arg(name: &str, value: IrValue) -> IrNamedArg {
        IrNamedArg {
            name: name.to_string(),
            name_span: span(0, name.len()),
            value,
            span: span(0, name.len()),
        }
    }

    #[test]
    fn native_dispatch_inventory_has_exactly_one_owner() {
        let mut registered = Vec::new();

        for builtin in builtins::regular_builtins() {
            assert_eq!(
                native_dispatch_owner(builtin.name),
                Some(NativeDispatchOwner::RegularScalar),
                "regular builtin {} has no unique owner",
                builtin.name
            );
            assert!(
                registered.iter().all(|(name, _)| *name != builtin.name),
                "{} is registered by more than one native owner",
                builtin.name
            );
            registered.push((builtin.name, NativeDispatchOwner::RegularScalar));
        }

        for inventory in bespoke_native_owners() {
            for &name in inventory.names {
                assert_eq!(
                    native_dispatch_owner(name),
                    Some(inventory.owner),
                    "{name} does not have one unique native owner"
                );
                assert!(
                    registered
                        .iter()
                        .all(|(registered_name, _)| *registered_name != name),
                    "{name} is registered by more than one native owner"
                );
                registered.push((name, inventory.owner));
            }
        }

        for &name in deferred_native_names() {
            assert_eq!(
                native_dispatch_owner(name),
                None,
                "deferred name {name} must not be a supported native owner"
            );
        }
    }

    fn collection_call(
        evaluator: &Evaluator,
        name: &str,
        positional_args: &[IrValue],
        named_args: &[IrNamedArg],
        operation_span: &SourceSpan,
        diagnostics: &mut Vec<Diagnostic>,
        context: &mut EvaluationContext<'_>,
    ) -> CallOutcome {
        evaluator.evaluate_call_value(
            name,
            positional_args,
            named_args,
            None,
            None,
            operation_span,
            diagnostics,
            context,
        )
    }

    fn text_paragraph(content: &str) -> IrNode {
        IrNode::Paragraph {
            content: vec![IrInline::Text {
                content: content.to_string(),
                span: span(0, content.len()),
            }],
            span: span(0, content.len()),
        }
    }

    fn text_inline(content: &str) -> IrInline {
        IrInline::Text {
            content: content.to_string(),
            span: span(0, content.len()),
        }
    }

    fn if_call(name: &str, condition: IrValue, body: Vec<IrNode>) -> IrNode {
        IrNode::FunctionCall {
            name: name.to_string(),
            positional_args: vec![condition],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: Some(body),
            span: span(0, 1),
        }
    }

    fn inline_if_call(name: &str, condition: IrValue, inline_body: Vec<IrInline>) -> IrInline {
        IrInline::DirectiveCall {
            name: name.to_string(),
            positional_args: vec![condition],
            named_args: Vec::new(),
            body: Some(inline_body),
            span: span(0, 1),
        }
    }

    fn doc(nodes: Vec<IrNode>) -> IrDocument {
        IrDocument {
            nodes,
            metadata: scribium_ir::IrMetadata::default(),
        }
    }

    fn evaluate(nodes: Vec<IrNode>) -> Vec<IrNode> {
        Evaluator::new().evaluate(&doc(nodes)).0.nodes
    }

    fn chain_segment(
        name: &str,
        start: usize,
        end: usize,
        positional_args: Vec<IrValue>,
    ) -> IrCallSegment {
        IrCallSegment {
            name: name.to_string(),
            name_span: span(start, start + name.len() + usize::from(start == 0)),
            positional_args,
            named_args: Vec::new(),
            span: span(start, end),
        }
    }

    fn chain_node(head: IrCallSegment, chain: Vec<IrCallSegment>) -> IrNode {
        let span = span(
            head.span.start,
            chain
                .last()
                .map_or(head.span.end, |segment| segment.span.end),
        );
        IrNode::ChainedFunctionCall {
            head,
            chain,
            body: None,
            span,
        }
    }

    fn chain_node_with_body(
        head: IrCallSegment,
        chain: Vec<IrCallSegment>,
        body: Vec<IrNode>,
    ) -> IrNode {
        let span = span(
            head.span.start,
            chain
                .last()
                .map_or(head.span.end, |segment| segment.span.end),
        );
        IrNode::ChainedFunctionCall {
            head,
            chain,
            body: Some(body),
            span,
        }
    }

    fn call_value(name: &str, positional_args: Vec<IrValue>) -> IrValue {
        IrValue::Content(vec![IrNode::FunctionCall {
            name: name.to_string(),
            positional_args,
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 1),
        }])
    }

    fn lambda_parameter(name: &str, start: usize) -> IrParameter {
        IrParameter {
            name: name.to_string(),
            name_span: span(start, start + name.len()),
            span: span(start, start + name.len() + 1),
            optional: false,
        }
    }

    fn let_call(
        value: Option<IrValue>,
        lambda_parameters: Option<Vec<IrParameter>>,
        body: Option<Vec<IrNode>>,
    ) -> IrNode {
        IrNode::FunctionCall {
            name: "let".to_string(),
            positional_args: value.into_iter().collect(),
            named_args: Vec::new(),
            lambda_parameters,
            body,
            span: span(0, 10),
        }
    }

    fn let_value(
        value: IrValue,
        lambda_parameters: Option<Vec<IrParameter>>,
        body: Vec<IrNode>,
    ) -> IrValue {
        IrValue::Content(vec![let_call(Some(value), lambda_parameters, Some(body))])
    }

    fn foreach_call(
        value: IrValue,
        lambda_parameters: Option<Vec<IrParameter>>,
        body: Vec<IrNode>,
    ) -> IrNode {
        IrNode::FunctionCall {
            name: "foreach".to_string(),
            positional_args: vec![value],
            named_args: Vec::new(),
            lambda_parameters,
            body: Some(body),
            span: span(0, 20),
        }
    }

    fn transform_callable(parameters: Option<Vec<IrParameter>>, body: Vec<IrNode>) -> IrValue {
        IrValue::Callable(IrCallable {
            parameters,
            body,
            span: span(50, 60),
            capture: None,
        })
    }

    fn component_value(component_span: SourceSpan) -> IrValue {
        IrValue::Component(IrComponent::Stacked(IrStackedComponent {
            layout: IrStackedLayout::Column,
            main_axis_alignment: IrMainAxisAlignment::Start,
            cross_axis_alignment: IrCrossAxisAlignment::Center,
            row_gap: Some(IrSize {
                value: 10.0,
                unit: IrSizeUnit::Px,
            }),
            column_gap: None,
            children: vec![text_paragraph("component child")],
            span: component_span,
        }))
    }

    fn assert_paragraph_text(nodes: &[IrNode], expected: &str) {
        let [IrNode::Paragraph { content, .. }] = nodes else {
            panic!("expected one paragraph, got {nodes:?}");
        };
        let [IrInline::Text { content, .. }] = content.as_slice() else {
            panic!("expected one text fragment, got {content:?}");
        };
        assert_eq!(content, expected);
    }

    #[test]
    fn let_explicit_parameter_returns_scalar() {
        let nodes = evaluate(vec![let_call(
            Some(IrValue::Number(5.0)),
            Some(vec![lambda_parameter("n", 20)]),
            Some(vec![var_ref("n")]),
        )]);
        assert_paragraph_text(&nodes, "5");
    }

    #[test]
    fn let_implicit_parameter_returns_scalar() {
        let nodes = evaluate(vec![let_call(
            Some(IrValue::String("Quarkdown".to_string())),
            None,
            Some(vec![var_ref("1")]),
        )]);
        assert_paragraph_text(&nodes, "Quarkdown");
    }

    #[test]
    fn let_preserves_scalar_result_in_nested_value_context() {
        let outer = IrNode::FunctionCall {
            name: "multiply".to_string(),
            positional_args: vec![
                let_value(
                    IrValue::Number(5.0),
                    Some(vec![lambda_parameter("n", 20)]),
                    vec![var_ref("n")],
                ),
                IrValue::Number(2.0),
            ],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 20),
        };
        let nodes = evaluate(vec![outer]);
        assert_paragraph_text(&nodes, "10");
    }

    #[test]
    fn let_returns_structured_content_and_composes_in_source_order() {
        let nodes = evaluate(vec![let_call(
            Some(IrValue::String("value".to_string())),
            Some(vec![lambda_parameter("name", 20)]),
            Some(vec![text_paragraph("First"), text_paragraph("Second")]),
        )]);
        assert_eq!(
            nodes,
            vec![text_paragraph("First"), text_paragraph("Second")]
        );
    }

    #[test]
    fn let_reads_parent_variable_and_function() {
        let declaration = IrNode::FunctionDeclaration {
            name: IrValue::Identifier("decorate".to_string()),
            parameters: vec![lambda_parameter("value", 5)],
            body: vec![IrNode::FunctionCall {
                name: "uppercase".to_string(),
                positional_args: vec![call_value("value", Vec::new())],
                named_args: Vec::new(),
                lambda_parameters: None,
                body: None,
                span: span(0, 1),
            }],
            span: span(0, 1),
        };
        let nodes = evaluate(vec![
            var_declaration("prefix", IrValue::String("Hello".to_string())),
            declaration,
            let_call(
                Some(IrValue::String("world".to_string())),
                Some(vec![lambda_parameter("name", 20)]),
                Some(vec![
                    var_ref("prefix"),
                    IrNode::FunctionCall {
                        name: "decorate".to_string(),
                        positional_args: vec![call_value("name", Vec::new())],
                        named_args: Vec::new(),
                        lambda_parameters: None,
                        body: None,
                        span: span(0, 1),
                    },
                ]),
            ),
        ]);
        assert_eq!(nodes.len(), 2);
        assert_paragraph_text(&nodes[..1], "Hello");
        assert_paragraph_text(&nodes[1..], "WORLD");
    }

    #[test]
    fn let_shadows_parent_and_local_variables_do_not_leak() {
        let nodes = evaluate(vec![
            var_declaration("name", IrValue::String("outer".to_string())),
            let_call(
                Some(IrValue::String("inner".to_string())),
                Some(vec![lambda_parameter("name", 20)]),
                Some(vec![var_ref("name")]),
            ),
            var_ref("name"),
        ]);
        assert_eq!(nodes.len(), 2);
        assert_paragraph_text(&nodes[..1], "inner");
        assert_paragraph_text(&nodes[1..], "outer");

        let nodes = evaluate(vec![
            var_declaration("x", IrValue::String("outer".to_string())),
            let_call(
                Some(IrValue::String("inner".to_string())),
                Some(vec![lambda_parameter("value", 20)]),
                Some(vec![
                    var_declaration("x", IrValue::String("local".to_string())),
                    var_ref("x"),
                ]),
            ),
            var_ref("x"),
        ]);
        assert_eq!(nodes.len(), 2);
        assert_paragraph_text(&nodes[0..1], "local");
        assert_paragraph_text(&nodes[1..2], "outer");
    }

    #[test]
    fn let_local_function_does_not_leak() {
        let local = IrNode::FunctionDeclaration {
            name: IrValue::Identifier("local".to_string()),
            parameters: Vec::new(),
            body: vec![text_paragraph("inside")],
            span: span(30, 35),
        };
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![let_call(
            Some(IrValue::String("hello".to_string())),
            Some(vec![lambda_parameter("value", 20)]),
            Some(vec![local]),
        )]);
        assert!(nodes.is_empty());
        assert!(diagnostics.is_empty());

        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![
            let_call(
                Some(IrValue::String("hello".to_string())),
                Some(vec![lambda_parameter("value", 20)]),
                Some(vec![IrNode::FunctionDeclaration {
                    name: IrValue::Identifier("local".to_string()),
                    parameters: Vec::new(),
                    body: vec![text_paragraph("inside")],
                    span: span(30, 35),
                }]),
            ),
            var_ref("local"),
        ]);
        assert!(diagnostics.is_empty());
        let [IrNode::FunctionCall { name, .. }] = nodes.as_slice() else {
            panic!("expected unresolved local function reference, got {nodes:?}")
        };
        assert_eq!(name, "local");
    }

    #[test]
    fn nested_let_uses_nearest_implicit_scope() {
        let nested = let_call(
            Some(IrValue::Content(vec![var_ref("1")])),
            None,
            Some(vec![var_ref("1")]),
        );
        let nodes = evaluate(vec![let_call(
            Some(IrValue::String("outer".to_string())),
            None,
            Some(vec![nested]),
        )]);
        assert_paragraph_text(&nodes, "outer");
    }

    #[test]
    fn explicit_let_masks_outer_implicit_scope() {
        let nested = let_call(
            Some(IrValue::String("inner".to_string())),
            Some(vec![lambda_parameter("value", 40)]),
            Some(vec![var_ref("1")]),
        );
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![let_call(
            Some(IrValue::String("outer".to_string())),
            None,
            Some(vec![nested]),
        )]);
        assert!(nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3003");
    }

    #[test]
    fn foreach_returns_a_typed_collection_before_output_materialization() {
        let evaluator = Evaluator::new();
        let range = IrValue::Range(IrRange {
            start: Some(2),
            end: Some(4),
            span: span(0, 5),
        });
        let body = vec![var_ref("n")];
        let parameters = vec![lambda_parameter("n", 10)];
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let outcome = evaluator.evaluate_call_value(
            "foreach",
            &[range],
            &[],
            Some(CallBody::Block(&body)),
            Some(&parameters),
            &span(0, 10),
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(
            outcome,
            CallOutcome::Value(IrValue::Collection(values))
                if values == vec![
                    IrValue::Number(2.0),
                    IrValue::Number(3.0),
                    IrValue::Number(4.0),
                ]
        ));
    }

    #[test]
    fn collection_transforms_share_typed_iterable_and_callable_paths() {
        let evaluator = Evaluator::new();
        let operation_span = span(0, 40);
        let range = IrValue::Range(IrRange {
            start: Some(-2),
            end: Some(2),
            span: span(1, 6),
        });
        let identity = transform_callable(None, vec![var_ref("1")]);
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();

        let mapped = evaluator.evaluate_call_value(
            "map",
            std::slice::from_ref(&range),
            &[named_arg("by", identity.clone())],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(
            mapped,
            CallOutcome::Value(IrValue::Collection(values))
                if values == (-2..=2).map(|value| IrValue::Number(f64::from(value))).collect::<Vec<_>>()
        ));

        let predicate = transform_callable(
            None,
            vec![IrNode::FunctionCall {
                name: "isnone".to_string(),
                positional_args: vec![call_value("1", Vec::new())],
                named_args: Vec::new(),
                lambda_parameters: None,
                body: None,
                span: span(10, 20),
            }],
        );
        let filter_input =
            IrValue::Collection(vec![IrValue::None, IrValue::Number(-1.0), IrValue::None]);
        let filtered = evaluator.evaluate_call_value(
            "filter",
            std::slice::from_ref(&filter_input),
            &[named_arg("by", predicate)],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(
            filtered,
            CallOutcome::Value(IrValue::Collection(values))
                if values == vec![IrValue::None, IrValue::None]
        ));

        let sorted = evaluator.evaluate_call_value(
            "sorted",
            &[IrValue::Collection(vec![
                IrValue::Number(3.0),
                IrValue::Number(1.0),
                IrValue::Number(2.0),
                IrValue::Number(1.0),
            ])],
            &[],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(
            sorted,
            CallOutcome::Value(IrValue::Collection(values))
                if values == vec![
                    IrValue::Number(1.0),
                    IrValue::Number(1.0),
                    IrValue::Number(2.0),
                    IrValue::Number(3.0),
                ]
        ));
    }

    #[test]
    fn transforms_support_pair_dictionary_and_nested_typed_values() {
        let evaluator = Evaluator::new();
        let dictionary = IrValue::Dictionary(IrDictionary {
            entries: vec![
                IrPair {
                    first: Box::new(IrValue::String("a".to_string())),
                    second: Box::new(IrValue::Number(3.0)),
                    span: span(1, 5),
                },
                IrPair {
                    first: Box::new(IrValue::String("b".to_string())),
                    second: Box::new(IrValue::Number(1.0)),
                    span: span(6, 10),
                },
                IrPair {
                    first: Box::new(IrValue::String("c".to_string())),
                    second: Box::new(IrValue::Number(1.0)),
                    span: span(11, 15),
                },
            ],
            span: span(0, 10),
        });
        let parameters = vec![lambda_parameter("key", 20), lambda_parameter("value", 24)];
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let mapped = evaluator.evaluate_call_value(
            "map",
            std::slice::from_ref(&dictionary),
            &[],
            Some(CallBody::Block(&[var_ref("value")])),
            Some(&parameters),
            &span(0, 30),
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(
            mapped,
            CallOutcome::Value(IrValue::Collection(values))
                if values == vec![
                    IrValue::Number(3.0),
                    IrValue::Number(1.0),
                    IrValue::Number(1.0),
                ]
        ));

        let sorted = evaluator.evaluate_call_value(
            "sorted",
            &[dictionary],
            &[named_arg(
                "by",
                transform_callable(Some(parameters), vec![var_ref("value")]),
            )],
            None,
            None,
            &span(0, 30),
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let CallOutcome::Value(IrValue::Collection(values)) = sorted else {
            panic!("expected sorted collection")
        };
        assert!(matches!(values[0], IrValue::Pair(_)));
        assert!(matches!(values[1], IrValue::Pair(_)));
        let IrValue::Pair(first) = &values[0] else {
            unreachable!()
        };
        assert_eq!(*first.second, IrValue::Number(1.0));
        let IrValue::Pair(second) = &values[1] else {
            unreachable!()
        };
        let IrValue::Pair(third) = &values[2] else {
            unreachable!()
        };
        assert_eq!(*second.first, IrValue::String("c".to_string()));
        assert_eq!(*third.first, IrValue::String("a".to_string()));
    }

    #[test]
    fn sorted_supports_typed_keys_and_fails_closed_for_unsupported_keys() {
        let evaluator = Evaluator::new();
        let operation_span = span(0, 40);
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();

        let strings = evaluator.evaluate_call_value(
            "sorted",
            &[IrValue::Collection(vec![
                IrValue::String("b".to_string()),
                IrValue::String("a".to_string()),
            ])],
            &[],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(
            strings,
            CallOutcome::Value(IrValue::Collection(values))
                if values == vec![
                    IrValue::String("a".to_string()),
                    IrValue::String("b".to_string()),
                ]
        ));

        let nan_sorted = evaluator.evaluate_call_value(
            "sorted",
            &[IrValue::Collection(vec![
                IrValue::Number(1.0),
                IrValue::Number(f64::NAN),
                IrValue::Number(0.0),
            ])],
            &[],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        let CallOutcome::Value(IrValue::Collection(values)) = nan_sorted else {
            panic!("expected NaN sort result")
        };
        assert_eq!(values[0], IrValue::Number(0.0));
        assert_eq!(values[1], IrValue::Number(1.0));
        assert!(matches!(values[2], IrValue::Number(value) if value.is_nan()));

        diagnostics.clear();
        let mixed = evaluator.evaluate_call_value(
            "sorted",
            &[IrValue::Collection(vec![
                IrValue::Number(1.0),
                IrValue::String("1".to_string()),
            ])],
            &[],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(mixed, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("heterogeneous"));

        diagnostics.clear();
        let none = evaluator.evaluate_call_value(
            "sorted",
            &[IrValue::Collection(vec![IrValue::None])],
            &[],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(none, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn transform_failures_are_atomic_and_predicates_are_boolean_only() {
        let evaluator = Evaluator::new();
        let operation_span = span(0, 40);
        let failing = transform_callable(
            None,
            vec![IrNode::FunctionCall {
                name: "multiply".to_string(),
                positional_args: vec![IrValue::Boolean(true), call_value("1", Vec::new())],
                named_args: Vec::new(),
                lambda_parameters: None,
                body: None,
                span: span(12, 20),
            }],
        );
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let mapped = evaluator.evaluate_call_value(
            "map",
            &[IrValue::Collection(vec![
                IrValue::Number(1.0),
                IrValue::Number(2.0),
            ])],
            &[named_arg("by", failing)],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(mapped, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");

        diagnostics.clear();
        let invalid_predicate = transform_callable(
            None,
            vec![IrNode::Paragraph {
                content: vec![text_inline("not boolean")],
                span: span(20, 31),
            }],
        );
        let filtered = evaluator.evaluate_call_value(
            "filter",
            &[IrValue::Collection(vec![IrValue::Number(1.0)])],
            &[named_arg("by", invalid_predicate)],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(filtered, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("Boolean"));

        diagnostics.clear();
        let endless = evaluator.evaluate_call_value(
            "map",
            &[IrValue::Range(IrRange {
                start: Some(1),
                end: None,
                span: span(5, 8),
            })],
            &[named_arg(
                "by",
                transform_callable(None, vec![var_ref("1")]),
            )],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(endless, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].primary, Some(span(5, 8)));
    }

    #[test]
    fn first_class_callable_captures_definition_values_and_applies_caller_overlay() {
        let evaluator = Evaluator::new();
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let span = span(0, 50);
        assert!(matches!(
            evaluator.evaluate_call_value(
                "var",
                &[
                    IrValue::Identifier("offset".to_string()),
                    IrValue::Number(10.0)
                ],
                &[],
                None,
                None,
                &span,
                &mut diagnostics,
                &mut context,
            ),
            CallOutcome::NoValue
        ));
        let callable = transform_callable(
            None,
            vec![IrNode::FunctionCall {
                name: "sum".to_string(),
                positional_args: vec![
                    call_value("1", Vec::new()),
                    call_value("offset", Vec::new()),
                ],
                named_args: Vec::new(),
                lambda_parameters: None,
                body: None,
                span,
            }],
        );
        assert!(matches!(
            evaluator.evaluate_call_value(
                "var",
                &[IrValue::Identifier("add_offset".to_string()), callable],
                &[],
                None,
                None,
                &span,
                &mut diagnostics,
                &mut context,
            ),
            CallOutcome::NoValue
        ));
        assert!(matches!(
            evaluator.evaluate_call_value(
                "offset",
                &[IrValue::Number(20.0)],
                &[],
                None,
                None,
                &span,
                &mut diagnostics,
                &mut context,
            ),
            CallOutcome::NoValue
        ));
        let result = evaluator.evaluate_call_value(
            "map",
            &[IrValue::Collection(vec![IrValue::Number(1.0)])],
            &[named_arg("by", call_value("add_offset", Vec::new()))],
            None,
            None,
            &span,
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(
            result,
            CallOutcome::Value(IrValue::Collection(values))
                if values == vec![IrValue::Number(21.0)]
        ));

        let wrong_arity = evaluator.evaluate_call_value(
            "map",
            &[IrValue::Collection(vec![IrValue::Number(1.0)])],
            &[named_arg(
                "by",
                transform_callable(
                    Some(vec![lambda_parameter("a", 1), lambda_parameter("b", 2)]),
                    vec![var_ref("a")],
                ),
            )],
            None,
            None,
            &span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(wrong_arity, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    }

    #[test]
    fn dynamic_range_returns_typed_signed_truncated_endpoints() {
        let evaluator = Evaluator::new();
        let operation_span = span(0, 20);
        let cases = [
            (
                vec![IrValue::Number(1.9), IrValue::Number(3.9)],
                Some(1),
                Some(3),
            ),
            (
                vec![IrValue::Number(-3.9), IrValue::Number(-1.1)],
                Some(-3),
                Some(-1),
            ),
            (
                vec![IrValue::Number(-0.9), IrValue::Number(0.9)],
                Some(0),
                Some(0),
            ),
        ];
        for (positional, start, end) in cases {
            let mut diagnostics = Vec::new();
            let mut context = EvaluationContext::new();
            let outcome = evaluator.evaluate_call_value(
                "range",
                &positional,
                &[],
                None,
                None,
                &operation_span,
                &mut diagnostics,
                &mut context,
            );
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            assert!(matches!(
                outcome,
                CallOutcome::Value(IrValue::Range(IrRange { start: actual_start, end: actual_end, .. }))
                    if actual_start == start && actual_end == end
            ));
        }

        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let outcome = evaluator.evaluate_call_value(
            "range",
            &[],
            &[named_arg("to", IrValue::Number(3.0))],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(
            outcome,
            CallOutcome::Value(IrValue::Range(IrRange {
                start: None,
                end: Some(3),
                ..
            }))
        ));

        let equivalent_forms = [
            (vec![IrValue::Number(2.0), IrValue::Number(4.0)], Vec::new()),
            (
                vec![IrValue::Number(2.0)],
                vec![named_arg("to", IrValue::Number(4.0))],
            ),
            (
                Vec::new(),
                vec![
                    named_arg("from", IrValue::Number(2.0)),
                    named_arg("to", IrValue::Number(4.0)),
                ],
            ),
        ];
        for (positional, named) in equivalent_forms {
            let mut diagnostics = Vec::new();
            let mut context = EvaluationContext::new();
            let outcome = evaluator.evaluate_call_value(
                "range",
                &positional,
                &named,
                None,
                None,
                &operation_span,
                &mut diagnostics,
                &mut context,
            );
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            assert!(matches!(
                outcome,
                CallOutcome::Value(IrValue::Range(IrRange {
                    start: Some(2),
                    end: Some(4),
                    ..
                }))
            ));
        }

        for (positional, named, expected) in [
            (
                Vec::new(),
                Vec::new(),
                IrRange {
                    start: None,
                    end: None,
                    span: operation_span,
                },
            ),
            (
                vec![IrValue::Number(2.0)],
                Vec::new(),
                IrRange {
                    start: Some(2),
                    end: None,
                    span: operation_span,
                },
            ),
            (
                Vec::new(),
                vec![named_arg("from", IrValue::Number(2.0))],
                IrRange {
                    start: Some(2),
                    end: None,
                    span: operation_span,
                },
            ),
        ] {
            let mut diagnostics = Vec::new();
            let mut context = EvaluationContext::new();
            let outcome = evaluator.evaluate_call_value(
                "range",
                &positional,
                &named,
                None,
                None,
                &operation_span,
                &mut diagnostics,
                &mut context,
            );
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            let CallOutcome::Value(IrValue::Range(actual)) = outcome else {
                panic!("expected typed Range")
            };
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn dynamic_range_number_conversion_matches_upstream_edges() {
        for (number, expected) in [
            (f64::NAN, 0),
            (f64::NEG_INFINITY, i32::MIN),
            (f64::INFINITY, i32::MAX),
            ((i32::MIN as f64) - 1.0, i32::MIN),
            ((i32::MAX as f64) + 1.0, i32::MAX),
            (f64::from(i32::MIN), i32::MIN),
            (f64::from(i32::MAX), i32::MAX),
        ] {
            assert_eq!(
                number_to_range_endpoint(&InvocationValue::static_value(IrValue::Number(number))),
                Ok(expected)
            );
        }
        assert!(
            number_to_range_endpoint(&InvocationValue::static_value(IrValue::Boolean(true)))
                .is_err()
        );
        assert_eq!(
            number_to_range_endpoint(&InvocationValue::dynamic_value(IrValue::String(
                "3".to_string()
            ))),
            Ok(3)
        );
    }

    #[test]
    fn range_materialization_handles_signed_and_left_open_bounds_once() {
        let evaluator = Evaluator::new();
        for (range, expected) in [
            (
                IrRange {
                    start: Some(-3),
                    end: Some(-1),
                    span: span(0, 5),
                },
                vec![-3.0, -2.0, -1.0],
            ),
            (
                IrRange {
                    start: Some(-3),
                    end: Some(3),
                    span: span(0, 5),
                },
                (-3..=3).map(f64::from).collect(),
            ),
            (
                IrRange {
                    start: None,
                    end: Some(3),
                    span: span(0, 4),
                },
                vec![1.0, 2.0, 3.0],
            ),
        ] {
            let mut diagnostics = Vec::new();
            let Ok(elements) = evaluator.coerce_iterable(
                InvocationValue::static_value(IrValue::Range(range)),
                &span(0, 10),
                &mut diagnostics,
            ) else {
                panic!("finite ranges materialize");
            };
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            assert_eq!(
                elements,
                expected
                    .into_iter()
                    .map(IrValue::Number)
                    .collect::<Vec<_>>()
            );
        }

        for range in [
            IrRange {
                start: None,
                end: Some(0),
                span: span(0, 3),
            },
            IrRange {
                start: None,
                end: Some(-2),
                span: span(0, 4),
            },
            IrRange {
                start: Some(4),
                end: Some(2),
                span: span(0, 4),
            },
        ] {
            let mut diagnostics = Vec::new();
            let Ok(elements) = evaluator.coerce_iterable(
                InvocationValue::static_value(IrValue::Range(range)),
                &span(0, 10),
                &mut diagnostics,
            ) else {
                panic!("descending or below-default ranges are empty");
            };
            assert!(elements.is_empty());
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }

        let mut diagnostics = Vec::new();
        let result = evaluator.coerce_iterable(
            InvocationValue::static_value(IrValue::Range(IrRange {
                start: Some(3),
                end: None,
                span: span(10, 13),
            })),
            &span(0, 20),
            &mut diagnostics,
        );
        assert!(matches!(result, Err(CallOutcome::Failed)));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].primary, Some(span(10, 13)));
    }

    #[test]
    fn materialization_limit_is_checked_before_range_allocation() {
        let evaluator = Evaluator::with_limits(EvaluationLimits {
            max_materialized_elements: 3,
            max_evaluation_depth: 256,
        });

        let mut diagnostics = Vec::new();
        let at_limit = evaluator
            .coerce_iterable(
                InvocationValue::static_value(IrValue::Range(IrRange {
                    start: Some(1),
                    end: Some(3),
                    span: span(10, 15),
                })),
                &span(0, 20),
                &mut diagnostics,
            )
            .expect("the exact materialization limit is valid");
        assert_eq!(at_limit.len(), 3);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let repeated_independent = evaluator
            .coerce_iterable(
                InvocationValue::static_value(IrValue::Range(IrRange {
                    start: Some(10),
                    end: Some(12),
                    span: span(30, 35),
                })),
                &span(0, 40),
                &mut diagnostics,
            )
            .expect("per-operation limits reset for an independent range");
        assert_eq!(repeated_independent.len(), 3);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let mut diagnostics = Vec::new();
        let over_limit = evaluator.coerce_iterable(
            InvocationValue::static_value(IrValue::Range(IrRange {
                start: Some(1),
                end: Some(4),
                span: span(50, 55),
            })),
            &span(0, 60),
            &mut diagnostics,
        );
        assert!(matches!(over_limit, Err(CallOutcome::Failed)));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3005");
        assert_eq!(
            diagnostics[0].message,
            "materialized element limit exceeded: requested 4, maximum is 3"
        );
        assert_eq!(diagnostics[0].primary, Some(span(50, 55)));

        let mut diagnostics = Vec::new();
        let huge = evaluator.coerce_iterable(
            InvocationValue::static_value(IrValue::Range(IrRange {
                start: Some(i32::MIN),
                end: Some(i32::MAX),
                span: span(70, 80),
            })),
            &span(0, 90),
            &mut diagnostics,
        );
        assert!(matches!(huge, Err(CallOutcome::Failed)));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3005");
        assert_eq!(diagnostics[0].primary, Some(span(70, 80)));
    }

    #[test]
    fn descending_empty_range_passes_even_when_materialization_limit_is_zero() {
        let evaluator = Evaluator::with_limits(EvaluationLimits {
            max_materialized_elements: 0,
            max_evaluation_depth: 256,
        });
        let mut diagnostics = Vec::new();
        let values = evaluator
            .coerce_iterable(
                InvocationValue::static_value(IrValue::Range(IrRange {
                    start: Some(3),
                    end: Some(1),
                    span: span(0, 4),
                })),
                &span(0, 4),
                &mut diagnostics,
            )
            .expect("descending ranges retain their empty semantics");
        assert!(values.is_empty());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    fn function_declaration(name: &str, body: Vec<IrNode>, start: usize) -> IrNode {
        IrNode::FunctionDeclaration {
            name: IrValue::Identifier(name.to_string()),
            parameters: Vec::new(),
            body,
            span: span(start, start + name.len()),
        }
    }

    fn function_call(name: &str, start: usize) -> IrNode {
        IrNode::FunctionCall {
            name: name.to_string(),
            positional_args: Vec::new(),
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(start, start + name.len()),
        }
    }

    #[test]
    fn nested_function_evaluation_at_depth_limit_passes() {
        let evaluator = Evaluator::with_limits(EvaluationLimits {
            max_materialized_elements: 16,
            max_evaluation_depth: 2,
        });
        let document = doc(vec![
            function_declaration("outer", vec![function_call("inner", 20)], 0),
            function_declaration("inner", vec![text_paragraph("ok")], 10),
            function_call("outer", 30),
        ]);

        let (result, diagnostics) = evaluator.evaluate(&document);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_paragraph_text(&result.nodes, "ok");
    }

    #[test]
    fn direct_and_indirect_recursion_fail_at_depth_limit_and_restore_siblings() {
        let evaluator = Evaluator::with_limits(EvaluationLimits {
            max_materialized_elements: 16,
            max_evaluation_depth: 3,
        });
        let direct = doc(vec![
            function_declaration("loop", vec![function_call("loop", 10)], 0),
            function_call("loop", 20),
            var_declaration("after", IrValue::String("usable".to_string())),
            var_ref("after"),
        ]);
        let (direct_result, direct_diagnostics) = evaluator.evaluate(&direct);
        assert_eq!(direct_diagnostics.len(), 1, "{direct_diagnostics:?}");
        assert_eq!(direct_diagnostics[0].code, "E3005");
        assert_eq!(direct_diagnostics[0].primary, Some(span(10, 14)));
        assert_paragraph_text(&direct_result.nodes, "usable");

        let indirect = doc(vec![
            function_declaration("first", vec![function_call("second", 40)], 30),
            function_declaration("second", vec![function_call("first", 50)], 45),
            function_call("first", 60),
        ]);
        let (indirect_result, indirect_diagnostics) = evaluator.evaluate(&indirect);
        assert!(indirect_result.nodes.is_empty());
        assert_eq!(indirect_diagnostics.len(), 1, "{indirect_diagnostics:?}");
        assert_eq!(indirect_diagnostics[0].code, "E3005");
        assert_eq!(indirect_diagnostics[0].primary, Some(span(40, 46)));
    }

    #[test]
    fn dynamic_range_remains_typed_inside_collection_and_pair_values() {
        let evaluator = Evaluator::new();
        let range = IrValue::Range(IrRange {
            start: Some(2),
            end: Some(4),
            span: span(0, 5),
        });
        let collection = IrValue::Collection(vec![range.clone()]);
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let outcome = evaluator.evaluate_call_value(
            "getat",
            &[collection, IrValue::Number(1.0)],
            &[],
            None,
            None,
            &span(0, 10),
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(outcome, CallOutcome::Value(value) if value == range));

        let pair = IrValue::Pair(IrPair {
            first: Box::new(range),
            second: Box::new(IrValue::String("value".to_string())),
            span: span(0, 10),
        });
        let outcome = evaluator.evaluate_call_value(
            "first",
            &[pair],
            &[],
            None,
            None,
            &span(0, 10),
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(outcome, CallOutcome::Value(IrValue::Range(_))));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn dynamic_range_argument_binding_is_checked_before_evaluation() {
        let evaluator = Evaluator::new();
        for named in [
            vec![named_arg("unknown", IrValue::Number(1.0))],
            vec![
                named_arg("from", IrValue::Number(1.0)),
                named_arg("from", IrValue::Number(2.0)),
            ],
        ] {
            let mut diagnostics = Vec::new();
            let mut context = EvaluationContext::new();
            let outcome = evaluator.evaluate_call_value(
                "range",
                &[],
                &named,
                None,
                None,
                &span(0, 20),
                &mut diagnostics,
                &mut context,
            );
            assert!(matches!(outcome, CallOutcome::Failed));
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        }

        let failing = call_value(
            "multiply",
            vec![IrValue::Boolean(true), IrValue::Number(2.0)],
        );
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let outcome = evaluator.evaluate_call_value(
            "range",
            &[failing],
            &[],
            None,
            None,
            &span(0, 20),
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(outcome, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    }

    #[test]
    fn collection_access_operations_preserve_recursive_types_and_dictionary_pairs() {
        let evaluator = Evaluator::new();
        let pair = IrValue::Pair(IrPair {
            first: Box::new(IrValue::String("key".to_string())),
            second: Box::new(IrValue::Boolean(true)),
            span: span(10, 20),
        });
        let dictionary = IrValue::Dictionary(IrDictionary {
            entries: vec![IrPair {
                first: Box::new(IrValue::String("first".to_string())),
                second: Box::new(IrValue::Collection(vec![IrValue::Number(2.0)])),
                span: span(21, 30),
            }],
            span: span(21, 30),
        });
        let collection = IrValue::Collection(vec![
            IrValue::Number(1.0),
            IrValue::Content(vec![text_paragraph("content")]),
            pair.clone(),
            dictionary.clone(),
        ]);
        let operation_span = span(0, 40);

        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let outcome = evaluator.evaluate_call_value(
            "size",
            &[],
            &[named_arg("of", collection.clone())],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(outcome, CallOutcome::Value(IrValue::Number(4.0))));

        let outcome = evaluator.evaluate_call_value(
            "first",
            std::slice::from_ref(&collection),
            &[],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(outcome, CallOutcome::Value(IrValue::Number(1.0))));

        let outcome = evaluator.evaluate_call_value(
            "getat",
            &[collection.clone(), IrValue::Number(2.0)],
            &[],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(
            outcome,
            CallOutcome::Value(IrValue::Content(nodes))
                if nodes == vec![text_paragraph("content")]
        ));

        let outcome = evaluator.evaluate_call_value(
            "last",
            &[collection],
            &[],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(outcome, CallOutcome::Value(value) if value == dictionary));

        let outcome = evaluator.evaluate_call_value(
            "getat",
            &[
                IrValue::Dictionary(IrDictionary {
                    entries: vec![IrPair {
                        first: Box::new(IrValue::String("a".to_string())),
                        second: Box::new(IrValue::Number(1.0)),
                        span: span(41, 45),
                    }],
                    span: span(41, 45),
                }),
                IrValue::Number(1.0),
            ],
            &[],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        let CallOutcome::Value(entry) = outcome else {
            panic!("expected a typed dictionary Pair")
        };
        assert!(matches!(entry, IrValue::Pair(_)));

        let outcome = evaluator.evaluate_call_value(
            "first",
            &[entry],
            &[],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(
            outcome,
            CallOutcome::Value(IrValue::String(value)) if value == "a"
        ));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn collection_access_indexing_matches_one_based_empty_and_invalid_boundaries() {
        let evaluator = Evaluator::new();
        let values = IrValue::Collection(vec![
            IrValue::String("first".to_string()),
            IrValue::String("second".to_string()),
        ]);
        let operation_span = span(0, 20);

        for index in [0.0, -1.0, 3.0, 9_007_199_254_740_992.0] {
            let mut diagnostics = Vec::new();
            let mut context = EvaluationContext::new();
            let outcome = evaluator.evaluate_call_value(
                "getat",
                &[values.clone(), IrValue::Number(index)],
                &[],
                None,
                None,
                &operation_span,
                &mut diagnostics,
                &mut context,
            );
            assert!(matches!(outcome, CallOutcome::Value(IrValue::None)));
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }

        let empty = IrValue::Range(IrRange {
            start: Some(4),
            end: Some(2),
            span: operation_span,
        });
        for name in ["first", "last"] {
            let mut diagnostics = Vec::new();
            let mut context = EvaluationContext::new();
            let outcome = evaluator.evaluate_call_value(
                name,
                std::slice::from_ref(&empty),
                &[],
                None,
                None,
                &operation_span,
                &mut diagnostics,
                &mut context,
            );
            assert!(matches!(outcome, CallOutcome::Value(IrValue::None)));
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }

        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let outcome = evaluator.evaluate_call_value(
            "getat",
            &[empty, IrValue::Number(1.0)],
            &[named_arg("orelse", IrValue::Boolean(true))],
            None,
            None,
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(
            outcome,
            CallOutcome::Value(IrValue::Boolean(true))
        ));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        for index in [1.5, f64::NAN, f64::INFINITY] {
            let mut diagnostics = Vec::new();
            let mut context = EvaluationContext::new();
            let outcome = evaluator.evaluate_call_value(
                "getat",
                &[values.clone(), IrValue::Number(index)],
                &[],
                None,
                None,
                &operation_span,
                &mut diagnostics,
                &mut context,
            );
            assert!(matches!(outcome, CallOutcome::Failed));
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        }
    }

    #[test]
    fn collection_second_and_third_share_one_based_iterable_access() {
        let evaluator = Evaluator::new();
        let operation_span = span(0, 20);
        let values = IrValue::Collection(vec![
            IrValue::String("one".to_string()),
            IrValue::Number(2.0),
            IrValue::Boolean(true),
        ]);
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();

        for (name, expected) in [
            ("second", IrValue::Number(2.0)),
            ("third", IrValue::Boolean(true)),
        ] {
            let outcome = collection_call(
                &evaluator,
                name,
                std::slice::from_ref(&values),
                &[],
                &operation_span,
                &mut diagnostics,
                &mut context,
            );
            assert!(matches!(outcome, CallOutcome::Value(value) if value == expected));
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }

        for value in [
            IrValue::Collection(Vec::new()),
            IrValue::Collection(vec![IrValue::Number(1.0)]),
        ] {
            for name in ["second", "third"] {
                let outcome = collection_call(
                    &evaluator,
                    name,
                    std::slice::from_ref(&value),
                    &[],
                    &operation_span,
                    &mut diagnostics,
                    &mut context,
                );
                assert!(matches!(outcome, CallOutcome::Value(IrValue::None)));
                assert!(diagnostics.is_empty(), "{diagnostics:?}");
            }
        }

        let pair = IrValue::Pair(IrPair {
            first: Box::new(IrValue::String("key".to_string())),
            second: Box::new(IrValue::Boolean(true)),
            span: span(21, 31),
        });
        let outcome = collection_call(
            &evaluator,
            "second",
            std::slice::from_ref(&pair),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(
            outcome,
            CallOutcome::Value(IrValue::Boolean(true))
        ));

        let getat = collection_call(
            &evaluator,
            "getat",
            &[values.clone(), IrValue::Number(2.0)],
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(
            getat,
            CallOutcome::Value(IrValue::Number(value)) if value == 2.0
        ));
        let getat = collection_call(
            &evaluator,
            "getat",
            &[values.clone(), IrValue::Number(3.0)],
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(getat, CallOutcome::Value(IrValue::Boolean(true))));

        let dictionary = IrValue::Dictionary(IrDictionary {
            entries: vec![
                IrPair {
                    first: Box::new(IrValue::String("a".to_string())),
                    second: Box::new(IrValue::Number(1.0)),
                    span: span(32, 36),
                },
                IrPair {
                    first: Box::new(IrValue::String("b".to_string())),
                    second: Box::new(IrValue::Number(2.0)),
                    span: span(37, 41),
                },
            ],
            span: span(32, 41),
        });
        let outcome = collection_call(
            &evaluator,
            "third",
            std::slice::from_ref(&dictionary),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(outcome, CallOutcome::Value(IrValue::None)));
        let outcome = collection_call(
            &evaluator,
            "second",
            std::slice::from_ref(&dictionary),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(
            outcome,
            CallOutcome::Value(IrValue::Pair(pair))
                if matches!(*pair.second, IrValue::Number(value) if value == 2.0)
        ));

        for (range, expected) in [
            (
                IrValue::Range(IrRange {
                    start: Some(-2),
                    end: Some(1),
                    span: span(42, 47),
                }),
                IrValue::Number(-1.0),
            ),
            (
                IrValue::Range(IrRange {
                    start: None,
                    end: Some(3),
                    span: span(48, 51),
                }),
                IrValue::Number(2.0),
            ),
        ] {
            let outcome = collection_call(
                &evaluator,
                "second",
                std::slice::from_ref(&range),
                &[],
                &operation_span,
                &mut diagnostics,
                &mut context,
            );
            assert!(matches!(outcome, CallOutcome::Value(value) if value == expected));
        }
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn collection_distinct_and_groupvalues_are_stable_and_typed() {
        let evaluator = Evaluator::new();
        let operation_span = span(0, 80);
        let pair_one = IrValue::Pair(IrPair {
            first: Box::new(IrValue::String("key".to_string())),
            second: Box::new(IrValue::Number(1.0)),
            span: span(1, 5),
        });
        let pair_two = IrValue::Pair(IrPair {
            first: Box::new(IrValue::String("key".to_string())),
            second: Box::new(IrValue::Number(1.0)),
            span: span(20, 24),
        });
        let dictionary_one = IrValue::Dictionary(IrDictionary {
            entries: vec![IrPair {
                first: Box::new(IrValue::String("a".to_string())),
                second: Box::new(IrValue::Number(1.0)),
                span: span(25, 29),
            }],
            span: span(25, 29),
        });
        let dictionary_two = IrValue::Dictionary(IrDictionary {
            entries: vec![IrPair {
                first: Box::new(IrValue::String("a".to_string())),
                second: Box::new(IrValue::Number(1.0)),
                span: span(30, 34),
            }],
            span: span(30, 34),
        });
        let nested = IrValue::Collection(vec![IrValue::String("nested".to_string())]);
        let input = IrValue::Collection(vec![
            IrValue::Number(1.0),
            IrValue::Number(1.0),
            IrValue::String("1".to_string()),
            IrValue::Boolean(true),
            IrValue::None,
            IrValue::Number(f64::NAN),
            IrValue::Number(f64::NAN),
            IrValue::Number(-0.0),
            IrValue::Number(0.0),
            pair_one.clone(),
            pair_two,
            nested.clone(),
            nested,
            dictionary_one.clone(),
            dictionary_two,
        ]);
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let empty_distinct = collection_call(
            &evaluator,
            "distinct",
            &[IrValue::Collection(Vec::new())],
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(
            empty_distinct,
            CallOutcome::Value(IrValue::Collection(values)) if values.is_empty()
        ));
        let distinct = collection_call(
            &evaluator,
            "distinct",
            std::slice::from_ref(&input),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        let CallOutcome::Value(IrValue::Collection(distinct_values)) = distinct else {
            panic!("expected distinct collection")
        };
        assert_eq!(distinct_values.len(), 10);
        assert!(matches!(distinct_values[0], IrValue::Number(1.0)));
        assert!(matches!(distinct_values[1], IrValue::String(ref value) if value == "1"));
        assert!(matches!(distinct_values[2], IrValue::Boolean(true)));
        assert!(matches!(distinct_values[3], IrValue::None));
        assert!(matches!(distinct_values[4], IrValue::Number(value) if value.is_nan()));
        assert!(matches!(distinct_values[5], IrValue::Number(value) if value == -0.0));
        assert!(matches!(distinct_values[6], IrValue::Number(value) if value == 0.0));
        assert_eq!(distinct_values[7], pair_one);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let dictionary_input = IrValue::Dictionary(IrDictionary {
            entries: vec![
                IrPair {
                    first: Box::new(IrValue::String("a".to_string())),
                    second: Box::new(IrValue::Number(1.0)),
                    span: span(35, 39),
                },
                IrPair {
                    first: Box::new(IrValue::String("b".to_string())),
                    second: Box::new(IrValue::Number(2.0)),
                    span: span(40, 44),
                },
            ],
            span: span(35, 44),
        });
        let distinct_dictionary = collection_call(
            &evaluator,
            "distinct",
            std::slice::from_ref(&dictionary_input),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(
            matches!(distinct_dictionary, CallOutcome::Value(IrValue::Collection(values)) if values.len() == 2 && matches!(&values[0], IrValue::Pair(pair) if matches!(*pair.first, IrValue::String(ref value) if value == "a")))
        );

        let grouped_dictionary = collection_call(
            &evaluator,
            "groupvalues",
            std::slice::from_ref(&dictionary_input),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(
            matches!(grouped_dictionary, CallOutcome::Value(IrValue::Collection(groups)) if groups.len() == 2 && groups.iter().all(|group| matches!(group, IrValue::Collection(values) if values.len() == 1)))
        );

        let range = IrValue::Range(IrRange {
            start: Some(1),
            end: Some(3),
            span: span(40, 44),
        });
        let range_distinct = collection_call(
            &evaluator,
            "distinct",
            std::slice::from_ref(&range),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(
            matches!(range_distinct, CallOutcome::Value(IrValue::Collection(values)) if values == [IrValue::Number(1.0), IrValue::Number(2.0), IrValue::Number(3.0)])
        );
        let range_groups = collection_call(
            &evaluator,
            "groupvalues",
            std::slice::from_ref(&range),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(
            matches!(range_groups, CallOutcome::Value(IrValue::Collection(groups)) if groups.len() == 3 && groups.iter().all(|group| matches!(group, IrValue::Collection(values) if values.len() == 1)))
        );

        let callable = IrValue::Callable(IrCallable {
            parameters: None,
            body: Vec::new(),
            span: span(45, 49),
            capture: None,
        });
        let callable_distinct = collection_call(
            &evaluator,
            "distinct",
            &[IrValue::Collection(vec![
                callable.clone(),
                callable.clone(),
            ])],
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(
            matches!(callable_distinct, CallOutcome::Value(IrValue::Collection(values)) if values.len() == 1)
        );
        let content_distinct = collection_call(
            &evaluator,
            "distinct",
            &[IrValue::Collection(vec![
                IrValue::Content(Vec::new()),
                IrValue::Content(Vec::new()),
            ])],
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(
            matches!(content_distinct, CallOutcome::Value(IrValue::Collection(values)) if values.len() == 1)
        );

        let grouped_input = IrValue::Collection(vec![
            IrValue::String("A".to_string()),
            IrValue::String("B".to_string()),
            IrValue::String("A".to_string()),
            IrValue::String("C".to_string()),
            IrValue::String("B".to_string()),
        ]);
        let grouped = collection_call(
            &evaluator,
            "groupvalues",
            std::slice::from_ref(&grouped_input),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(
            matches!(grouped, CallOutcome::Value(IrValue::Collection(ref groups)) if groups == &[
                IrValue::Collection(vec![
                    IrValue::String("A".to_string()),
                    IrValue::String("A".to_string()),
                ]),
                IrValue::Collection(vec![
                    IrValue::String("B".to_string()),
                    IrValue::String("B".to_string()),
                ]),
                IrValue::Collection(vec![IrValue::String("C".to_string())]),
            ])
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let repeated = collection_call(
            &evaluator,
            "distinct",
            std::slice::from_ref(&grouped_input),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        let repeated = match repeated {
            CallOutcome::Value(value) => value,
            _ => panic!("expected repeated distinct result"),
        };
        let repeated_again = collection_call(
            &evaluator,
            "distinct",
            std::slice::from_ref(&grouped_input),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert_eq!(
            repeated,
            match repeated_again {
                CallOutcome::Value(value) => value,
                _ => panic!("expected deterministic distinct result"),
            }
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let pair_groups = collection_call(
            &evaluator,
            "groupvalues",
            &[IrValue::Pair(IrPair {
                first: Box::new(IrValue::String("same".to_string())),
                second: Box::new(IrValue::String("same".to_string())),
                span: span(81, 86),
            })],
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(
            matches!(pair_groups, CallOutcome::Value(IrValue::Collection(ref groups)) if groups.len() == 1 && matches!(&groups[0], IrValue::Collection(values) if values.len() == 2))
        );

        let empty_groups = collection_call(
            &evaluator,
            "groupvalues",
            &[IrValue::Collection(Vec::new())],
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(
            empty_groups,
            CallOutcome::Value(IrValue::Collection(values)) if values.is_empty()
        ));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn collection_reversed_uses_the_shared_materialized_sequence() {
        let evaluator = Evaluator::new();
        let operation_span = span(0, 30);
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let cases = [
            (
                IrValue::Collection(Vec::new()),
                IrValue::Collection(Vec::new()),
            ),
            (
                IrValue::Collection(vec![IrValue::String("one".to_string())]),
                IrValue::Collection(vec![IrValue::String("one".to_string())]),
            ),
            (
                IrValue::Collection(vec![
                    IrValue::Collection(vec![IrValue::Number(1.0)]),
                    IrValue::Number(2.0),
                ]),
                IrValue::Collection(vec![
                    IrValue::Number(2.0),
                    IrValue::Collection(vec![IrValue::Number(1.0)]),
                ]),
            ),
            (
                IrValue::Pair(IrPair {
                    first: Box::new(IrValue::String("a".to_string())),
                    second: Box::new(IrValue::String("b".to_string())),
                    span: span(31, 36),
                }),
                IrValue::Collection(vec![
                    IrValue::String("b".to_string()),
                    IrValue::String("a".to_string()),
                ]),
            ),
            (
                IrValue::Dictionary(IrDictionary {
                    entries: vec![
                        IrPair {
                            first: Box::new(IrValue::String("a".to_string())),
                            second: Box::new(IrValue::Number(1.0)),
                            span: span(53, 57),
                        },
                        IrPair {
                            first: Box::new(IrValue::String("b".to_string())),
                            second: Box::new(IrValue::Number(2.0)),
                            span: span(58, 62),
                        },
                    ],
                    span: span(53, 62),
                }),
                IrValue::Collection(vec![
                    IrValue::Pair(IrPair {
                        first: Box::new(IrValue::String("b".to_string())),
                        second: Box::new(IrValue::Number(2.0)),
                        span: span(58, 62),
                    }),
                    IrValue::Pair(IrPair {
                        first: Box::new(IrValue::String("a".to_string())),
                        second: Box::new(IrValue::Number(1.0)),
                        span: span(53, 57),
                    }),
                ]),
            ),
            (
                IrValue::Range(IrRange {
                    start: Some(-2),
                    end: Some(0),
                    span: span(37, 42),
                }),
                IrValue::Collection(vec![
                    IrValue::Number(0.0),
                    IrValue::Number(-1.0),
                    IrValue::Number(-2.0),
                ]),
            ),
            (
                IrValue::Range(IrRange {
                    start: None,
                    end: Some(3),
                    span: span(43, 46),
                }),
                IrValue::Collection(vec![
                    IrValue::Number(3.0),
                    IrValue::Number(2.0),
                    IrValue::Number(1.0),
                ]),
            ),
            (
                IrValue::Range(IrRange {
                    start: Some(4),
                    end: Some(2),
                    span: span(47, 52),
                }),
                IrValue::Collection(Vec::new()),
            ),
        ];
        for (input, expected) in cases {
            let outcome = collection_call(
                &evaluator,
                "reversed",
                std::slice::from_ref(&input),
                &[],
                &operation_span,
                &mut diagnostics,
                &mut context,
            );
            assert!(matches!(outcome, CallOutcome::Value(value) if value == expected));
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }

        let endless_span = span(53, 56);
        let outcome = collection_call(
            &evaluator,
            "reversed",
            &[IrValue::Range(IrRange {
                start: Some(1),
                end: None,
                span: endless_span,
            })],
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(outcome, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].primary, Some(endless_span));
    }

    #[test]
    fn collection_sumall_and_average_follow_as_double_and_kotlin_average() {
        let evaluator = Evaluator::new();
        let operation_span = span(0, 30);
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let mixed = IrValue::Collection(vec![
            IrValue::Number(1.5),
            IrValue::Number(-2.0),
            IrValue::String("3.5".to_string()),
            IrValue::Boolean(true),
            IrValue::None,
            IrValue::String("invalid".to_string()),
        ]);

        let sum = collection_call(
            &evaluator,
            "sumall",
            std::slice::from_ref(&mixed),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(sum, CallOutcome::Value(IrValue::Number(value)) if value == 3.0));
        let average = collection_call(
            &evaluator,
            "average",
            std::slice::from_ref(&mixed),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(average, CallOutcome::Value(IrValue::Number(value)) if value == 0.5));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let empty = IrValue::Collection(Vec::new());
        let sum = collection_call(
            &evaluator,
            "sumall",
            std::slice::from_ref(&empty),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(sum, CallOutcome::Value(IrValue::Number(value)) if value == 0.0));
        let average = collection_call(
            &evaluator,
            "average",
            std::slice::from_ref(&empty),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(average, CallOutcome::Value(IrValue::Number(value)) if value.is_nan()));

        let special = IrValue::Collection(vec![
            IrValue::Number(f64::INFINITY),
            IrValue::Number(f64::NEG_INFINITY),
        ]);
        let sum = collection_call(
            &evaluator,
            "sumall",
            std::slice::from_ref(&special),
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(sum, CallOutcome::Value(IrValue::Number(value)) if value.is_nan()));
        let average = collection_call(
            &evaluator,
            "average",
            &[IrValue::Collection(vec![IrValue::Number(f64::INFINITY)])],
            &[],
            &operation_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(
            matches!(average, CallOutcome::Value(IrValue::Number(value)) if value.is_infinite() && value.is_sign_positive())
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn collection_access_reuses_failure_outcomes_and_checks_length_conversion() {
        let evaluator = Evaluator::new();
        let operation_span = span(0, 20);
        let failing = call_value(
            "multiply",
            vec![IrValue::Boolean(true), IrValue::Number(2.0)],
        );
        let unresolved = call_value("unknown", Vec::new());

        for value in [failing, unresolved, IrValue::Boolean(true)] {
            let mut diagnostics = Vec::new();
            let mut context = EvaluationContext::new();
            let outcome = evaluator.evaluate_call_value(
                "size",
                &[value],
                &[],
                None,
                None,
                &operation_span,
                &mut diagnostics,
                &mut context,
            );
            assert!(matches!(outcome, CallOutcome::Failed));
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        }

        let mut diagnostics = Vec::new();
        assert!(matches!(
            exact_collection_length(usize::MAX, &operation_span, &mut diagnostics),
            Err(CallOutcome::Failed)
        ));
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn pair_evaluation_is_typed_recursive_and_atomic_on_child_failure() {
        let evaluator = Evaluator::new();
        let pair_span = span(10, 20);
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let outcome = evaluator.evaluate_call_value(
            "pair",
            &[
                IrValue::String("key".to_string()),
                IrValue::Collection(vec![IrValue::Number(1.0), IrValue::Boolean(true)]),
            ],
            &[],
            None,
            None,
            &pair_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(
            outcome,
            CallOutcome::Value(IrValue::Pair(IrPair { first, second, span }))
                if *first == IrValue::String("key".to_string())
                    && *second == IrValue::Collection(vec![
                        IrValue::Number(1.0),
                        IrValue::Boolean(true),
                    ])
                    && span == pair_span
        ));

        let failing = call_value(
            "multiply",
            vec![IrValue::Boolean(true), IrValue::Number(2.0)],
        );
        diagnostics.clear();
        let outcome = evaluator.evaluate_call_value(
            "pair",
            &[IrValue::Number(1.0), failing],
            &[],
            None,
            None,
            &pair_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(outcome, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    }

    #[test]
    fn dictionary_iteration_reuses_pair_items_and_explicit_destructuring() {
        let evaluator = Evaluator::new();
        let dictionary_span = span(0, 30);
        let dictionary = IrValue::Dictionary(IrDictionary {
            entries: vec![
                IrPair {
                    first: Box::new(IrValue::String("a".to_string())),
                    second: Box::new(IrValue::Number(1.0)),
                    span: span(5, 10),
                },
                IrPair {
                    first: Box::new(IrValue::String("b".to_string())),
                    second: Box::new(IrValue::Number(2.0)),
                    span: span(10, 15),
                },
            ],
            span: dictionary_span,
        });
        let parameters = vec![lambda_parameter("key", 20), lambda_parameter("value", 24)];
        let body = vec![var_ref("key"), var_ref("value")];
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let outcome = evaluator.evaluate_call_value(
            "foreach",
            &[dictionary],
            &[],
            Some(CallBody::Block(&body)),
            Some(&parameters),
            &dictionary_span,
            &mut diagnostics,
            &mut context,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let CallOutcome::Value(IrValue::Collection(values)) = outcome else {
            panic!("expected typed iteration result")
        };
        assert_eq!(values.len(), 2);
        assert!(matches!(
            &values[0],
            IrValue::Content(nodes) if nodes.len() == 2
        ));
        assert!(matches!(
            &values[1],
            IrValue::Content(nodes) if nodes.len() == 2
        ));
    }

    #[test]
    fn pair_destructuring_rejects_non_pair_items_without_coercion() {
        let evaluator = Evaluator::new();
        let parameters = vec![lambda_parameter("key", 20), lambda_parameter("value", 24)];
        let body = vec![var_ref("key")];
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let outcome = evaluator.evaluate_call_value(
            "foreach",
            &[IrValue::Collection(vec![IrValue::String(
                "invalid".to_string(),
            )])],
            &[],
            Some(CallBody::Block(&body)),
            Some(&parameters),
            &span(0, 20),
            &mut diagnostics,
            &mut context,
        );
        assert!(matches!(outcome, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("expected a Pair"));
    }

    #[test]
    fn block_materialization_of_mixed_collection_is_fail_fast_and_atomic() {
        let range_span = span(10, 14);
        let value = IrValue::Collection(vec![
            IrValue::Number(1.0),
            IrValue::Range(IrRange {
                start: Some(2),
                end: Some(4),
                span: range_span,
            }),
            IrValue::Number(5.0),
        ]);
        let mut diagnostics = Vec::new();
        let result =
            Evaluator::new().materialize_block_value(value, &span(0, 20), &mut diagnostics);
        assert!(matches!(result, Err(CallOutcome::Failed)));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].primary, Some(range_span));
    }

    #[test]
    fn block_materialization_of_nested_range_is_fail_fast_and_atomic() {
        let range_span = span(10, 14);
        let value = IrValue::Collection(vec![IrValue::Collection(vec![IrValue::Range(IrRange {
            start: Some(2),
            end: Some(4),
            span: range_span,
        })])]);
        let mut diagnostics = Vec::new();
        let result =
            Evaluator::new().materialize_block_value(value, &span(0, 20), &mut diagnostics);
        assert!(matches!(result, Err(CallOutcome::Failed)));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].primary, Some(range_span));
    }

    #[test]
    fn component_remains_typed_in_value_context_and_preserves_source_span() {
        let component_span = span(20, 44);
        let component = component_value(component_span);
        let mut diagnostics = Vec::new();
        let mut context = EvaluationContext::new();
        let outcome = Evaluator::new().evaluate_value(&component, &mut diagnostics, &mut context);

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(outcome, CallOutcome::Value(component.clone()));
        assert_eq!(value_source_span(&component, &span(0, 1)), component_span);
    }

    #[test]
    fn component_survives_variable_and_callable_value_flow() {
        let component = component_value(span(20, 44));
        let mut context = EvaluationContext::new();
        context.set_value("component".to_string(), component.clone());
        context.set_function_binding(
            "make".to_string(),
            LambdaParameters::Implicit,
            vec![var_ref("component")],
            span(50, 54),
            None,
        );
        let evaluator = Evaluator::new();
        let mut diagnostics = Vec::new();

        let variable_reference = call_value("component", Vec::new());
        assert_eq!(
            evaluator.evaluate_value(&variable_reference, &mut diagnostics, &mut context),
            CallOutcome::Value(component.clone())
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let outcome = evaluator.evaluate_call_value(
            "make",
            &[],
            &[],
            None,
            None,
            &span(60, 64),
            &mut diagnostics,
            &mut context,
        );
        assert_eq!(outcome, CallOutcome::Value(component));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn single_callable_component_result_remains_a_value() {
        let component = component_value(span(20, 44));
        let mut context = EvaluationContext::new();
        context.set_value("component".to_string(), component.clone());
        let mut diagnostics = Vec::new();

        let outcome = Evaluator::new().evaluate_callable_body_value(
            &[var_ref("component")],
            &mut diagnostics,
            &mut context,
        );

        assert_eq!(outcome, CallOutcome::Value(component));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn component_block_materialization_publishes_one_typed_node() {
        let component_span = span(20, 44);
        let component = component_value(component_span);
        let mut diagnostics = Vec::new();
        let result =
            Evaluator::new().materialize_block_value(component, &span(0, 1), &mut diagnostics);

        let Ok([IrNode::Component { component }]) = result.as_deref() else {
            panic!("expected one typed component node, got {result:?}");
        };
        assert_eq!(component.span(), component_span);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn component_variable_at_document_output_boundary_materializes_as_a_node() {
        let component_span = span(20, 44);
        let (nodes, diagnostics) = Evaluator::new().evaluate(&doc(vec![
            var_declaration("component", component_value(component_span)),
            var_ref("component"),
        ]));

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let [IrNode::Component { component }] = nodes.nodes.as_slice() else {
            panic!("expected one component node, got {:?}", nodes.nodes);
        };
        assert_eq!(component.span(), component_span);
    }

    #[test]
    fn component_inline_materialization_fails_with_empty_output() {
        let component_span = span(20, 44);
        let component = component_value(component_span);
        let mut diagnostics = Vec::new();
        let inlines = Evaluator::new().materialize_inline_value(
            Some(component),
            &span(0, 1),
            &mut diagnostics,
        );

        assert!(inlines.is_empty());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].primary, Some(component_span));
        assert!(diagnostics[0].message.contains("block-only"));
    }

    #[test]
    fn component_and_second_callable_output_preserve_both_nodes() {
        let component_span = span(20, 44);
        let component = component_value(component_span);
        let mut context = EvaluationContext::new();
        context.set_value("component".to_string(), component);
        let mut diagnostics = Vec::new();

        let outcome = Evaluator::new().evaluate_callable_body_value(
            &[var_ref("component"), text_paragraph("later output")],
            &mut diagnostics,
            &mut context,
        );

        let CallOutcome::Value(IrValue::Content(nodes)) = outcome else {
            panic!("expected composed content");
        };
        assert_eq!(nodes.len(), 2);
        assert!(matches!(nodes[0], IrNode::Component { .. }));
        assert!(matches!(nodes[1], IrNode::Paragraph { .. }));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn component_is_rejected_by_scalar_text_materialization() {
        let component_span = span(20, 44);
        let component = component_value(component_span);
        let mut diagnostics = Vec::new();
        let result = scalar_to_text(&component, span(0, 1), &mut diagnostics);

        assert!(matches!(result, Err(CallOutcome::Failed)));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].primary, Some(component_span));
        assert!(diagnostics[0].message.contains("scalar text"));
    }

    #[test]
    fn block_materialization_of_normal_collection_preserves_order() {
        let value = IrValue::Collection(vec![
            IrValue::Number(1.0),
            IrValue::Number(2.0),
            IrValue::Number(3.0),
        ]);
        let mut diagnostics = Vec::new();
        let nodes =
            match Evaluator::new().materialize_block_value(value, &span(0, 20), &mut diagnostics) {
                Ok(nodes) => nodes,
                Err(_) => panic!("normal Collection should materialize"),
            };
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(nodes.len(), 3);
        for (node, expected) in nodes.iter().zip(["1", "2", "3"]) {
            let IrNode::Paragraph { content, .. } = node else {
                panic!("expected scalar paragraph, got {node:?}")
            };
            assert!(matches!(
                content.as_slice(),
                [IrInline::Text { content, .. }] if content == expected
            ));
        }
    }

    #[test]
    fn foreach_empty_collection_does_not_invoke_the_body() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![foreach_call(
            IrValue::Collection(Vec::new()),
            None,
            vec![var_ref("2")],
        )]);
        assert!(nodes.is_empty());
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn foreach_nested_iterable_expression_flows_through_one_value_context() {
        let nested = foreach_call(
            IrValue::Range(IrRange {
                start: Some(1),
                end: Some(2),
                span: span(0, 4),
            }),
            None,
            vec![var_ref("1")],
        );
        let outer = foreach_call(IrValue::Content(vec![nested]), None, vec![var_ref("1")]);
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![outer]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_paragraph_text(&nodes[0..1], "1");
        assert_paragraph_text(&nodes[1..2], "2");
    }

    #[test]
    fn foreach_local_function_does_not_leak_to_parent() {
        let local = IrNode::FunctionDeclaration {
            name: IrValue::Identifier("local".to_string()),
            parameters: Vec::new(),
            body: vec![text_paragraph("inside")],
            span: span(20, 25),
        };
        let foreach = IrNode::FunctionCall {
            name: "foreach".to_string(),
            positional_args: vec![IrValue::Range(IrRange {
                start: Some(1),
                end: Some(2),
                span: span(0, 4),
            })],
            named_args: Vec::new(),
            lambda_parameters: Some(vec![lambda_parameter("n", 10)]),
            body: Some(vec![local, var_ref("n")]),
            span: span(0, 20),
        };
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![foreach, var_ref("local")]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(matches!(
            nodes.last(),
            Some(IrNode::FunctionCall { name, .. }) if name == "local"
        ));
    }

    #[test]
    fn let_missing_implicit_parameter_reports_original_span() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![let_call(
            Some(IrValue::String("value".to_string())),
            None,
            Some(vec![var_ref("2")]),
        )]);
        assert!(nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3003");
        assert_eq!(diagnostics[0].primary, Some(span(0, 1)));
    }

    #[test]
    fn let_arity_and_value_errors_are_deterministic() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![let_call(
            None,
            Some(vec![lambda_parameter("value", 20)]),
            Some(vec![var_ref("value")]),
        )]);
        assert!(nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].primary, Some(span(0, 10)));

        let call = IrNode::FunctionCall {
            name: "let".to_string(),
            positional_args: vec![IrValue::Number(1.0), IrValue::Number(2.0)],
            named_args: Vec::new(),
            lambda_parameters: Some(vec![lambda_parameter("value", 20)]),
            body: Some(vec![var_ref("value")]),
            span: span(0, 10),
        };
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![call]);
        assert!(nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);

        let call = let_call(
            Some(IrValue::Number(1.0)),
            Some(vec![
                lambda_parameter("first", 20),
                lambda_parameter("second", 30),
            ]),
            Some(vec![var_ref("first")]),
        );
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![call]);
        assert!(nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].primary, Some(span(20, 26)));
    }

    #[test]
    fn unknown_chain_callee_reports_a_segment_diagnostic() {
        let whole = span(0, 13);
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![IrNode::ChainedFunctionCall {
            head: IrCallSegment {
                name: "a".into(),
                name_span: span(0, 2),
                positional_args: vec![IrValue::Identifier("x".into())],
                named_args: Vec::new(),
                span: whole,
            },
            chain: vec![IrCallSegment {
                name: "b".into(),
                name_span: span(8, 9),
                positional_args: vec![IrValue::Identifier("y".into())],
                named_args: Vec::new(),
                span: span(8, 13),
            }],
            body: None,
            span: whole,
        }]);
        assert!(nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3001");
        assert_eq!(diagnostics[0].primary, Some(span(0, 2)));
    }

    #[test]
    fn unknown_middle_and_tail_segments_fail_at_their_names() {
        let cases = [
            (
                vec![
                    chain_segment(
                        "uppercase",
                        0,
                        17,
                        vec![IrValue::Identifier("hello".into())],
                    ),
                    chain_segment("unknown", 19, 28, Vec::new()),
                    chain_segment("lowercase", 30, 39, Vec::new()),
                ],
                span(19, 26),
            ),
            (
                vec![
                    chain_segment(
                        "uppercase",
                        0,
                        17,
                        vec![IrValue::Identifier("hello".into())],
                    ),
                    chain_segment("lowercase", 19, 29, Vec::new()),
                    chain_segment("unknown", 31, 40, Vec::new()),
                ],
                span(31, 38),
            ),
        ];

        for (segments, expected_span) in cases {
            let (nodes, diagnostics) = evaluate_with_diagnostics(vec![chain_node(
                segments[0].clone(),
                segments[1..].to_vec(),
            )]);
            assert!(nodes.is_empty());
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].code, "E3001");
            assert_eq!(diagnostics[0].primary, Some(expected_span));
        }
    }

    #[test]
    fn chain_arity_and_type_failures_are_deterministic() {
        let cases = [
            chain_node(
                chain_segment("uppercase", 0, 10, Vec::new()),
                vec![chain_segment("lowercase", 12, 21, Vec::new())],
            ),
            chain_node(
                chain_segment("sum", 0, 8, vec![IrValue::Boolean(true)]),
                vec![chain_segment(
                    "multiply",
                    10,
                    19,
                    vec![IrValue::Number(2.0)],
                )],
            ),
        ];
        for input in cases {
            let first = Evaluator::new().evaluate(&doc(vec![input.clone()]));
            let second = Evaluator::new().evaluate(&doc(vec![input]));
            assert!(first.0.nodes.is_empty());
            assert_eq!(first.1.len(), 1);
            assert_eq!(second.1.len(), 1);
            assert_eq!(first.1[0].code, "E3001");
            assert_eq!(first.1[0].message, second.1[0].message);
            assert_eq!(first.1[0].primary, second.1[0].primary);
        }
    }

    #[test]
    fn chain_value_flow_is_left_to_right_and_injects_first() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![chain_node(
            chain_segment(
                "sum",
                0,
                12,
                vec![IrValue::Number(10.0), IrValue::Number(5.0)],
            ),
            vec![chain_segment(
                "multiply",
                14,
                27,
                vec![IrValue::Number(2.0)],
            )],
        )]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_paragraph_text(&nodes, "30");
    }

    #[test]
    fn chain_zero_argument_segments_compose_scalar_values() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![chain_node(
            chain_segment(
                "uppercase",
                0,
                17,
                vec![IrValue::Identifier("hello".into())],
            ),
            vec![
                chain_segment("uppercase", 19, 28, Vec::new()),
                chain_segment("lowercase", 30, 39, Vec::new()),
            ],
        )]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_paragraph_text(&nodes, "hello");
    }

    #[test]
    fn chain_preserves_explicit_positional_arguments_after_previous_value() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![chain_node(
            chain_segment(
                "sum",
                0,
                12,
                vec![IrValue::Number(10.0), IrValue::Number(5.0)],
            ),
            vec![chain_segment(
                "multiply",
                14,
                27,
                vec![IrValue::Number(2.0)],
            )],
        )]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        // multiply receives [sum(10, 5), 2], not [2, sum(10, 5)].
        assert_paragraph_text(&nodes, "30");
    }

    #[test]
    fn chain_keeps_named_arguments_named_while_injecting_previous_value() {
        let mut segment = chain_segment("multiply", 14, 29, Vec::new());
        segment
            .named_args
            .push(named_arg("by", IrValue::Number(2.0)));
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![chain_node(
            chain_segment(
                "sum",
                0,
                12,
                vec![IrValue::Number(10.0), IrValue::Number(5.0)],
            ),
            vec![segment],
        )]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_paragraph_text(&nodes, "30");
    }

    #[test]
    fn final_chain_reassignment_is_a_legal_no_value_result() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![
            var_declaration("x", IrValue::Number(0.0)),
            chain_node(
                chain_segment(
                    "sum",
                    0,
                    12,
                    vec![IrValue::Number(1.0), IrValue::Number(2.0)],
                ),
                vec![chain_segment("x", 14, 15, Vec::new())],
            ),
            var_ref("x"),
        ]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_paragraph_text(&nodes, "3");
    }

    #[test]
    fn non_final_chain_reassignment_reports_no_value_and_stops() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![
            var_declaration("x", IrValue::Number(0.0)),
            chain_node(
                chain_segment(
                    "sum",
                    0,
                    12,
                    vec![IrValue::Number(1.0), IrValue::Number(2.0)],
                ),
                vec![
                    chain_segment("x", 14, 15, Vec::new()),
                    chain_segment("sum", 17, 25, vec![IrValue::Number(1.0)]),
                ],
            ),
            var_ref("x"),
        ]);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3001");
        assert_eq!(diagnostics[0].primary, Some(span(14, 15)));
        assert_paragraph_text(&nodes, "3");
    }

    #[test]
    fn nested_no_value_argument_reports_e3001_without_invoking_outer_call() {
        let nested_reassignment = IrValue::Content(vec![IrNode::FunctionCall {
            name: "x".to_string(),
            positional_args: vec![IrValue::Number(3.0)],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(7, 12),
        }]);
        let outer = IrNode::FunctionCall {
            name: "multiply".to_string(),
            positional_args: vec![nested_reassignment, IrValue::Number(2.0)],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 20),
        };
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![
            var_declaration("x", IrValue::Number(0.0)),
            outer,
            var_ref("x"),
        ]);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3001");
        assert_eq!(diagnostics[0].primary, Some(span(7, 12)));
        assert_paragraph_text(&nodes, "3");
    }

    #[test]
    fn nested_no_value_named_argument_reports_e3001_without_invoking_outer_call() {
        let nested_reassignment = IrValue::Content(vec![IrNode::FunctionCall {
            name: "x".to_string(),
            positional_args: vec![IrValue::Number(3.0)],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(9, 14),
        }]);
        let outer = IrNode::FunctionCall {
            name: "multiply".to_string(),
            positional_args: vec![IrValue::Number(2.0)],
            named_args: vec![named_arg("by", nested_reassignment)],
            lambda_parameters: None,
            body: None,
            span: span(0, 22),
        };
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![
            var_declaration("x", IrValue::Number(0.0)),
            outer,
            var_ref("x"),
        ]);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3001");
        assert_eq!(diagnostics[0].primary, Some(span(9, 14)));
        assert_paragraph_text(&nodes, "3");
    }

    #[test]
    fn nested_function_declaration_reports_no_value_once_at_its_span() {
        let declaration_span = span(10, 24);
        let declaration = IrValue::Content(vec![IrNode::FunctionDeclaration {
            name: IrValue::Identifier("declared".to_string()),
            parameters: Vec::new(),
            body: vec![text_paragraph("body")],
            span: declaration_span,
        }]);
        let outer = IrNode::FunctionCall {
            name: "sum".to_string(),
            positional_args: vec![declaration, IrValue::Number(1.0)],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 30),
        };

        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![outer]);
        assert!(nodes.is_empty());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3001");
        assert_eq!(diagnostics[0].primary, Some(declaration_span));
        assert!(diagnostics[0].message.contains("no value"));
    }

    #[test]
    fn failed_nested_call_propagates_without_a_duplicate_no_value_error() {
        let invalid_sum = call_value("sum", vec![IrValue::Boolean(true)]);
        let outer = IrNode::FunctionCall {
            name: "multiply".to_string(),
            positional_args: vec![invalid_sum, IrValue::Number(2.0)],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 20),
        };
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![outer]);
        assert!(nodes.is_empty());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3001");
        assert!(diagnostics[0]
            .message
            .contains("requires numeric arguments"));
    }

    #[test]
    fn malformed_nested_var_propagates_its_original_diagnostic_only() {
        let invalid_var = IrValue::Content(vec![IrNode::FunctionCall {
            name: "var".to_string(),
            positional_args: vec![
                IrValue::Identifier("bad name".to_string()),
                IrValue::Number(1.0),
            ],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(7, 18),
        }]);
        let outer = IrNode::FunctionCall {
            name: "multiply".to_string(),
            positional_args: vec![invalid_var, IrValue::Number(2.0)],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 20),
        };
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![outer]);
        assert!(nodes.is_empty());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "E3002");
        assert_eq!(diagnostics[0].primary, Some(span(7, 18)));
    }

    #[test]
    fn nested_call_and_chain_share_the_same_value_context() {
        let nested = IrNode::FunctionCall {
            name: "multiply".into(),
            positional_args: vec![
                call_value("sum", vec![IrValue::Number(10.0), IrValue::Number(5.0)]),
                IrValue::Number(2.0),
            ],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 1),
        };
        let chain = chain_node(
            chain_segment(
                "sum",
                0,
                12,
                vec![IrValue::Number(10.0), IrValue::Number(5.0)],
            ),
            vec![chain_segment(
                "multiply",
                14,
                27,
                vec![IrValue::Number(2.0)],
            )],
        );
        let (nested_nodes, nested_diagnostics) = Evaluator::new().evaluate(&doc(vec![nested]));
        let (chain_nodes, chain_diagnostics) = Evaluator::new().evaluate(&doc(vec![chain]));
        assert!(nested_diagnostics.is_empty(), "{nested_diagnostics:?}");
        assert!(chain_diagnostics.is_empty(), "{chain_diagnostics:?}");
        assert_paragraph_text(&nested_nodes.nodes, "30");
        assert_paragraph_text(&chain_nodes.nodes, "30");
    }

    #[test]
    fn nested_and_chained_case_transforms_share_dynamic_scalar_adaptation() {
        let nested = IrNode::FunctionCall {
            name: "lowercase".into(),
            positional_args: vec![call_value(
                "uppercase",
                vec![IrValue::Identifier("hello".into())],
            )],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 1),
        };
        let chain = chain_node(
            chain_segment(
                "uppercase",
                0,
                17,
                vec![IrValue::Identifier("hello".into())],
            ),
            vec![chain_segment("lowercase", 19, 28, Vec::new())],
        );
        let (nested_nodes, nested_diagnostics) = Evaluator::new().evaluate(&doc(vec![nested]));
        let (chain_nodes, chain_diagnostics) = Evaluator::new().evaluate(&doc(vec![chain]));
        assert!(nested_diagnostics.is_empty(), "{nested_diagnostics:?}");
        assert!(chain_diagnostics.is_empty(), "{chain_diagnostics:?}");
        assert_paragraph_text(&nested_nodes.nodes, "hello");
        assert_paragraph_text(&chain_nodes.nodes, "hello");
    }

    #[test]
    fn variable_values_remain_semantic_through_nested_and_chained_calls() {
        let nested = vec![
            var_declaration("myvar", IrValue::Boolean(true)),
            IrNode::FunctionCall {
                name: "uppercase".into(),
                positional_args: vec![call_value("myvar", Vec::new())],
                named_args: Vec::new(),
                lambda_parameters: None,
                body: None,
                span: span(0, 1),
            },
        ];
        let chained = vec![
            var_declaration("myvar", IrValue::Boolean(true)),
            chain_node(
                chain_segment("myvar", 0, 6, Vec::new()),
                vec![chain_segment("uppercase", 8, 18, Vec::new())],
            ),
        ];
        let (nested_nodes, nested_diagnostics) = Evaluator::new().evaluate(&doc(nested));
        let (chain_nodes, chain_diagnostics) = Evaluator::new().evaluate(&doc(chained));
        assert!(nested_diagnostics.is_empty(), "{nested_diagnostics:?}");
        assert!(chain_diagnostics.is_empty(), "{chain_diagnostics:?}");
        assert_paragraph_text(&nested_nodes.nodes, "TRUE");
        assert_paragraph_text(&chain_nodes.nodes, "TRUE");
    }

    #[test]
    fn false_final_conditional_chain_does_not_evaluate_its_body() {
        let chain = vec![
            var_declaration("flag", IrValue::Boolean(false)),
            var_declaration("x", IrValue::Identifier("before".into())),
            chain_node_with_body(
                chain_segment("flag", 0, 5, Vec::new()),
                vec![chain_segment("if", 7, 10, Vec::new())],
                vec![var_reassignment("x", IrValue::Identifier("after".into()))],
            ),
            var_ref("x"),
        ];
        let ordinary = vec![
            var_declaration("flag", IrValue::Boolean(false)),
            var_declaration("x", IrValue::Identifier("before".into())),
            if_call(
                "if",
                IrValue::Boolean(false),
                vec![var_reassignment("x", IrValue::Identifier("after".into()))],
            ),
            var_ref("x"),
        ];
        let (chain_nodes, chain_diagnostics) = Evaluator::new().evaluate(&doc(chain));
        let (ordinary_nodes, ordinary_diagnostics) = Evaluator::new().evaluate(&doc(ordinary));
        assert!(chain_diagnostics.is_empty(), "{chain_diagnostics:?}");
        assert!(ordinary_diagnostics.is_empty(), "{ordinary_diagnostics:?}");
        assert_paragraph_text(&chain_nodes.nodes, "before");
        assert_paragraph_text(&ordinary_nodes.nodes, "before");
    }

    #[test]
    fn false_final_inline_conditional_chain_does_not_evaluate_its_body() {
        let chain = vec![
            var_declaration("flag", IrValue::Boolean(false)),
            var_declaration("x", IrValue::Identifier("before".into())),
            IrNode::Paragraph {
                content: vec![
                    IrInline::ChainedDirectiveCall {
                        head: chain_segment("flag", 0, 5, Vec::new()),
                        chain: vec![chain_segment("if", 7, 10, Vec::new())],
                        body: Some(vec![IrInline::DirectiveCall {
                            name: "x".into(),
                            positional_args: vec![IrValue::Identifier("after".into())],
                            named_args: Vec::new(),
                            body: None,
                            span: span(0, 1),
                        }]),
                        span: span(0, 10),
                    },
                    inline_var_ref("x"),
                ],
                span: span(0, 10),
            },
        ];
        let ordinary = vec![
            var_declaration("flag", IrValue::Boolean(false)),
            var_declaration("x", IrValue::Identifier("before".into())),
            IrNode::Paragraph {
                content: vec![
                    inline_if_call(
                        "if",
                        IrValue::Boolean(false),
                        vec![IrInline::DirectiveCall {
                            name: "x".into(),
                            positional_args: vec![IrValue::Identifier("after".into())],
                            named_args: Vec::new(),
                            body: None,
                            span: span(0, 1),
                        }],
                    ),
                    inline_var_ref("x"),
                ],
                span: span(0, 10),
            },
        ];
        let (chain_nodes, chain_diagnostics) = Evaluator::new().evaluate(&doc(chain));
        let (ordinary_nodes, ordinary_diagnostics) = Evaluator::new().evaluate(&doc(ordinary));
        assert!(chain_diagnostics.is_empty(), "{chain_diagnostics:?}");
        assert!(ordinary_diagnostics.is_empty(), "{ordinary_diagnostics:?}");
        let text = |nodes: &IrDocument| match &nodes.nodes[0] {
            IrNode::Paragraph { content, .. } => content
                .iter()
                .filter_map(|inline| match inline {
                    IrInline::Text { content, .. } => Some(content.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            _ => String::new(),
        };
        assert_eq!(text(&chain_nodes), "before");
        assert_eq!(text(&ordinary_nodes), "before");
    }

    #[test]
    fn child_scope_inherits_parent_and_isolates_local_bindings() {
        let mut parent = EvaluationContext::new();
        parent.set_value("visible".into(), IrValue::String("parent".into()));
        parent.set_function("inherited".into(), vec!["value".into()]);

        let mut child = parent.child();
        assert_eq!(
            child.get("visible").map(VariableValue::to_value),
            Some(IrValue::String("parent".into()))
        );
        assert_eq!(
            child
                .get_function("inherited")
                .and_then(|binding| binding.parameters.explicit())
                .map(|parameters| parameters
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()),
            Some(vec!["value"])
        );
        child.set_value("local".into(), IrValue::String("child".into()));
        child.set_function("future".into(), vec!["value".into()]);

        assert!(parent.get("local").is_none());
        assert_eq!(
            child.get("local").map(VariableValue::to_value),
            Some(IrValue::String("child".into()))
        );
        assert_eq!(
            child
                .get_function("future")
                .and_then(|binding| binding.parameters.explicit())
                .map(|parameters| parameters
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()),
            Some(vec!["value"])
        );
        child.set_value("visible".into(), IrValue::String("shadowed".into()));
        child.set_function("inherited".into(), vec!["shadowed".into()]);
        assert_eq!(
            child.get("visible").map(VariableValue::to_value),
            Some(IrValue::String("shadowed".into()))
        );
        assert_eq!(
            parent.get("visible").map(VariableValue::to_value),
            Some(IrValue::String("parent".into()))
        );
        assert_eq!(
            child
                .get_function("inherited")
                .and_then(|binding| binding.parameters.explicit())
                .map(|parameters| parameters
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()),
            Some(vec!["shadowed"])
        );
        assert_eq!(
            parent
                .get_function("inherited")
                .and_then(|binding| binding.parameters.explicit())
                .map(|parameters| parameters
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()),
            Some(vec!["value"])
        );
    }

    #[test]
    fn if_true_keeps_block_body() {
        let nodes = evaluate(vec![if_call(
            "if",
            IrValue::Boolean(true),
            vec![text_paragraph("kept")],
        )]);
        assert_eq!(nodes, vec![text_paragraph("kept")]);
    }

    #[test]
    fn if_false_drops_block_body() {
        let nodes = evaluate(vec![if_call(
            "if",
            IrValue::Boolean(false),
            vec![text_paragraph("dropped")],
        )]);
        assert!(nodes.is_empty());
    }

    #[test]
    fn ifnot_true_drops_and_ifnot_false_keeps() {
        let keep = evaluate(vec![if_call(
            "ifnot",
            IrValue::Boolean(false),
            vec![text_paragraph("kept")],
        )]);
        assert_eq!(keep, vec![text_paragraph("kept")]);

        let drop = evaluate(vec![if_call(
            "ifnot",
            IrValue::Boolean(true),
            vec![text_paragraph("dropped")],
        )]);
        assert!(drop.is_empty());
    }

    #[test]
    fn boolean_identifiers_yes_no_true_false_case_insensitive() {
        for (literal, expected) in [
            ("yes", true),
            ("YES", true),
            ("true", true),
            ("True", true),
            ("no", false),
            ("No", false),
            ("false", false),
            ("FALSE", false),
        ] {
            let nodes = evaluate(vec![if_call(
                "if",
                IrValue::Identifier(literal.to_string()),
                vec![text_paragraph("content")],
            )]);
            if expected {
                assert_eq!(nodes, vec![text_paragraph("content")], "literal {literal}");
            } else {
                assert!(nodes.is_empty(), "literal {literal}");
            }
        }
    }

    #[test]
    fn missing_condition_reports_e3001_and_drops() {
        let call = IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: Vec::new(),
            named_args: Vec::new(),
            lambda_parameters: None,
            body: Some(vec![text_paragraph("content")]),
            span: span(3, 6),
        };
        let (result, diagnostics) = Evaluator::new().evaluate(&doc(vec![call]));
        assert!(result.nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3001");
        assert!(matches!(diagnostics[0].severity, Severity::Error));
        assert_eq!(diagnostics[0].primary, Some(span(3, 6)));
    }

    #[test]
    fn unresolvable_condition_reports_diagnostic() {
        for condition in [
            IrValue::Number(3.0),
            IrValue::String("maybe".to_string()),
            IrValue::Identifier("unknown".to_string()),
            IrValue::Content(vec![text_paragraph("content")]),
        ] {
            let display = format!("{condition:?}");
            let (result, diagnostics) = Evaluator::new().evaluate(&doc(vec![if_call(
                "if",
                condition.clone(),
                vec![text_paragraph("body")],
            )]));
            assert!(result.nodes.is_empty(), "condition {display}");
            assert_eq!(diagnostics.len(), 1, "condition {display}");
            assert_eq!(diagnostics[0].code, "E3001");
        }
    }

    #[test]
    fn nested_if_inside_block_body_is_evaluated() {
        let body = vec![
            text_paragraph("before"),
            if_call(
                "if",
                IrValue::Boolean(false),
                vec![text_paragraph("inner-dropped")],
            ),
            if_call(
                "if",
                IrValue::Boolean(true),
                vec![text_paragraph("inner-kept")],
            ),
        ];
        let nodes = evaluate(vec![if_call("if", IrValue::Boolean(true), body)]);
        assert_eq!(
            nodes,
            vec![text_paragraph("before"), text_paragraph("inner-kept"),]
        );
    }

    #[test]
    fn content_value_second_argument_replaces_call() {
        let call = IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: vec![
                IrValue::Boolean(true),
                IrValue::Content(vec![text_paragraph("arg content")]),
            ],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 1),
        };
        let nodes = evaluate(vec![call]);
        assert_eq!(nodes, vec![text_paragraph("arg content")]);
    }

    #[test]
    fn scalar_second_argument_becomes_text() {
        let call = IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: vec![
                IrValue::Boolean(true),
                IrValue::String("inline text".to_string()),
            ],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 1),
        };
        let nodes = evaluate(vec![call]);
        assert_eq!(
            nodes,
            vec![IrNode::Paragraph {
                content: vec![IrInline::Text {
                    content: "inline text".to_string(),
                    span: span(0, 1),
                }],
                span: span(0, 1),
            }]
        );
    }

    #[test]
    fn block_body_takes_priority_over_positional_content() {
        let call = IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: vec![
                IrValue::Boolean(true),
                IrValue::Content(vec![text_paragraph("from arg")]),
            ],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: Some(vec![text_paragraph("from body")]),
            span: span(0, 1),
        };
        let nodes = evaluate(vec![call]);
        assert_eq!(nodes, vec![text_paragraph("from body")]);
    }

    #[test]
    fn inline_if_replaces_call_with_inline_body_or_content() {
        let paragraph = IrNode::Paragraph {
            content: vec![
                text_inline("before "),
                inline_if_call("if", IrValue::Boolean(true), vec![text_inline("kept")]),
                text_inline(" after"),
            ],
            span: span(0, 1),
        };
        let nodes = evaluate(vec![paragraph]);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!("expected paragraph");
        };
        let rendered: Vec<&str> = content
            .iter()
            .map(|i| match i {
                IrInline::Text { content, .. } => content.as_str(),
                other => panic!("unexpected inline {other:?}"),
            })
            .collect();
        assert_eq!(rendered, vec!["before ", "kept", " after"]);
    }

    #[test]
    fn link_evaluates_content_inside_label() {
        let paragraph = IrNode::Paragraph {
            content: vec![IrInline::Link {
                content: vec![
                    text_inline("before "),
                    inline_if_call("if", IrValue::Boolean(true), vec![text_inline("kept")]),
                    text_inline(" after"),
                ],
                destination: "https://example.com".to_string(),
                title: None,
                span: span(0, 1),
            }],
            span: span(0, 1),
        };
        let nodes = evaluate(vec![paragraph]);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(content.len(), 1);
        let IrInline::Link {
            content,
            destination,
            title: _title,
            span: link_span,
        } = &content[0]
        else {
            panic!("expected link");
        };
        assert_eq!(destination, "https://example.com");
        assert_eq!(*link_span, span(0, 1));
        assert_eq!(
            content,
            &vec![
                text_inline("before "),
                text_inline("kept"),
                text_inline(" after")
            ]
        );
    }

    #[test]
    fn structures_recurse_through_evaluator_without_losing_semantics() {
        let document = doc(vec![
            IrNode::Blockquote {
                content: vec![if_call(
                    "if",
                    IrValue::Boolean(true),
                    vec![text_paragraph("quoted")],
                )],
                span: span(0, 10),
            },
            IrNode::UnorderedList {
                items: vec![IrListItem {
                    nodes: vec![if_call(
                        "if",
                        IrValue::Boolean(true),
                        vec![text_paragraph("task content")],
                    )],
                    task: Some(scribium_ir::IrTaskStatus::Completed),
                    span: span(10, 30),
                }],
                span: span(10, 30),
            },
            IrNode::Paragraph {
                content: vec![IrInline::Strikethrough {
                    content: vec![inline_if_call(
                        "if",
                        IrValue::Boolean(true),
                        vec![text_inline("struck")],
                    )],
                    span: span(30, 40),
                }],
                span: span(30, 40),
            },
            IrNode::Table {
                header: scribium_ir::IrTableRow {
                    cells: vec![scribium_ir::IrTableCell {
                        content: vec![text_inline("Header")],
                        alignment: scribium_ir::IrTableAlignment::Center,
                        span: span(40, 46),
                    }],
                    span: span(40, 46),
                },
                rows: vec![scribium_ir::IrTableRow {
                    cells: vec![scribium_ir::IrTableCell {
                        content: vec![inline_if_call(
                            "if",
                            IrValue::Boolean(true),
                            vec![text_inline("cell")],
                        )],
                        alignment: scribium_ir::IrTableAlignment::None,
                        span: span(46, 50),
                    }],
                    span: span(46, 50),
                }],
                span: span(40, 50),
            },
        ]);

        let (evaluated, diagnostics) = Evaluator::new().evaluate(&document);
        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let IrNode::Blockquote { content, .. } = &evaluated.nodes[0] else {
            panic!("expected blockquote")
        };
        assert_eq!(content, &vec![text_paragraph("quoted")]);

        let IrNode::UnorderedList { items, .. } = &evaluated.nodes[1] else {
            panic!("expected list")
        };
        assert_eq!(items[0].task, Some(scribium_ir::IrTaskStatus::Completed));
        assert_eq!(items[0].nodes, vec![text_paragraph("task content")]);

        let IrNode::Paragraph { content, .. } = &evaluated.nodes[2] else {
            panic!("expected paragraph")
        };
        assert_eq!(
            content,
            &vec![IrInline::Strikethrough {
                content: vec![text_inline("struck")],
                span: span(30, 40),
            }]
        );

        let IrNode::Table { header, rows, .. } = &evaluated.nodes[3] else {
            panic!("expected table")
        };
        assert_eq!(header.cells[0].content, vec![text_inline("Header")]);
        assert_eq!(rows[0].cells[0].content, vec![text_inline("cell")]);
    }

    #[test]
    fn inline_if_false_drops_call() {
        let paragraph = IrNode::Paragraph {
            content: vec![
                text_inline("before "),
                inline_if_call(
                    "ifnot",
                    IrValue::Boolean(true),
                    vec![text_inline("dropped")],
                ),
                text_inline(" after"),
            ],
            span: span(0, 1),
        };
        let nodes = evaluate(vec![paragraph]);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(
            content,
            &vec![text_inline("before "), text_inline(" after")]
        );
    }

    #[test]
    fn inline_call_scalar_second_argument_becomes_text() {
        let call = IrInline::DirectiveCall {
            name: "if".to_string(),
            positional_args: vec![IrValue::Boolean(true), IrValue::String("shown".to_string())],
            named_args: Vec::new(),
            body: None,
            span: span(0, 1),
        };
        let paragraph = IrNode::Paragraph {
            content: vec![text_inline("x "), call],
            span: span(0, 1),
        };
        let nodes = evaluate(vec![paragraph]);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!("expected paragraph");
        };
        assert_eq!(
            content,
            &vec![
                text_inline("x "),
                IrInline::Text {
                    content: "shown".to_string(),
                    span: span(0, 1),
                }
            ]
        );
    }

    #[test]
    fn non_conditional_calls_are_preserved_with_evaluated_bodies() {
        let call = IrNode::FunctionCall {
            name: "foo".to_string(),
            positional_args: Vec::new(),
            named_args: Vec::new(),
            lambda_parameters: None,
            body: Some(vec![if_call(
                "if",
                IrValue::Boolean(false),
                vec![text_paragraph("dropped")],
            )]),
            span: span(0, 1),
        };
        let nodes = evaluate(vec![call]);
        let IrNode::FunctionCall { name, body, .. } = &nodes[0] else {
            panic!("expected function call");
        };
        assert_eq!(name, "foo");
        assert!(body.as_ref().unwrap().is_empty());
    }

    #[test]
    fn evaluation_is_immutable_and_deterministic() {
        let call = if_call("if", IrValue::Boolean(true), vec![text_paragraph("kept")]);
        let input = doc(vec![call.clone()]);
        let first = Evaluator::new().evaluate(&input);
        assert_eq!(input.nodes, vec![call]);
        let second = Evaluator::new().evaluate(&input);
        assert_eq!(first.0, second.0);
        assert!(first.1.is_empty() && second.1.is_empty());
    }

    #[test]
    fn named_condition_argument_works() {
        let call = IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: Vec::new(),
            named_args: vec![named_arg("condition", IrValue::Boolean(true))],
            lambda_parameters: None,
            body: Some(vec![text_paragraph("kept")]),
            span: span(0, 1),
        };
        let nodes = evaluate(vec![call]);
        assert_eq!(nodes, vec![text_paragraph("kept")]);
    }

    #[test]
    fn named_condition_false_drops_body() {
        let call = IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: Vec::new(),
            named_args: vec![named_arg("condition", IrValue::Boolean(false))],
            lambda_parameters: None,
            body: Some(vec![text_paragraph("dropped")]),
            span: span(0, 1),
        };
        let nodes = evaluate(vec![call]);
        assert!(nodes.is_empty());
    }

    #[test]
    fn named_condition_ifnot_inverts() {
        let call = IrNode::FunctionCall {
            name: "ifnot".to_string(),
            positional_args: Vec::new(),
            named_args: vec![named_arg("condition", IrValue::Boolean(false))],
            lambda_parameters: None,
            body: Some(vec![text_paragraph("kept")]),
            span: span(0, 1),
        };
        let nodes = evaluate(vec![call]);
        assert_eq!(nodes, vec![text_paragraph("kept")]);
    }

    #[test]
    fn named_condition_identifier_yes_no() {
        for (ident, expected) in [("yes", true), ("YES", true), ("no", false), ("No", false)] {
            let call = IrNode::FunctionCall {
                name: "if".to_string(),
                positional_args: Vec::new(),
                named_args: vec![named_arg(
                    "condition",
                    IrValue::Identifier(ident.to_string()),
                )],
                lambda_parameters: None,
                body: Some(vec![text_paragraph("content")]),
                span: span(0, 1),
            };
            let nodes = evaluate(vec![call]);
            if expected {
                assert_eq!(nodes, vec![text_paragraph("content")], "ident {ident}");
            } else {
                assert!(nodes.is_empty(), "ident {ident}");
            }
        }
    }

    #[test]
    fn named_body_argument_works() {
        let call = IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: vec![IrValue::Boolean(true)],
            named_args: vec![named_arg(
                "body",
                IrValue::Content(vec![text_paragraph("from named body")]),
            )],
            lambda_parameters: None,
            body: None,
            span: span(0, 1),
        };
        let nodes = evaluate(vec![call]);
        assert_eq!(nodes, vec![text_paragraph("from named body")]);
    }

    #[test]
    fn named_body_scalar_argument_works() {
        let call = IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: vec![IrValue::Boolean(true)],
            named_args: vec![named_arg(
                "body",
                IrValue::String("scalar body".to_string()),
            )],
            lambda_parameters: None,
            body: None,
            span: span(0, 1),
        };
        let nodes = evaluate(vec![call]);
        assert_eq!(
            nodes,
            vec![IrNode::Paragraph {
                content: vec![IrInline::Text {
                    content: "scalar body".to_string(),
                    span: span(0, 1),
                }],
                span: span(0, 1),
            }]
        );
    }

    #[test]
    fn block_body_priority_over_named_body() {
        let call = IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: vec![IrValue::Boolean(true)],
            named_args: vec![named_arg(
                "body",
                IrValue::Content(vec![text_paragraph("from named body")]),
            )],
            lambda_parameters: None,
            body: Some(vec![text_paragraph("from indented body")]),
            span: span(0, 1),
        };
        let nodes = evaluate(vec![call]);
        assert_eq!(nodes, vec![text_paragraph("from indented body")]);
    }

    #[test]
    fn inline_named_condition_works() {
        let paragraph = IrNode::Paragraph {
            content: vec![
                text_inline("before "),
                IrInline::DirectiveCall {
                    name: "if".to_string(),
                    positional_args: Vec::new(),
                    named_args: vec![named_arg("condition", IrValue::Boolean(true))],
                    body: Some(vec![text_inline("kept")]),
                    span: span(0, 1),
                },
                text_inline(" after"),
            ],
            span: span(0, 1),
        };
        let nodes = evaluate(vec![paragraph]);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        let rendered: Vec<&str> = content
            .iter()
            .map(|i| match i {
                IrInline::Text { content, .. } => content.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(rendered, vec!["before ", "kept", " after"]);
    }

    #[test]
    fn inline_named_body_works() {
        let call = IrInline::DirectiveCall {
            name: "if".to_string(),
            positional_args: vec![IrValue::Boolean(true)],
            named_args: vec![named_arg(
                "body",
                IrValue::String("inline shown".to_string()),
            )],
            body: None,
            span: span(0, 1),
        };
        let paragraph = IrNode::Paragraph {
            content: vec![text_inline("x "), call],
            span: span(0, 1),
        };
        let nodes = evaluate(vec![paragraph]);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        assert_eq!(
            content,
            &vec![
                text_inline("x "),
                IrInline::Text {
                    content: "inline shown".to_string(),
                    span: span(0, 1),
                }
            ]
        );
    }

    #[test]
    fn named_condition_unresolvable_reports_e3001() {
        let call = IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: Vec::new(),
            named_args: vec![named_arg("condition", IrValue::Number(3.0))],
            lambda_parameters: None,
            body: Some(vec![text_paragraph("body")]),
            span: span(3, 6),
        };
        let (result, diagnostics) = Evaluator::new().evaluate(&doc(vec![call]));
        assert!(result.nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3001");
    }

    // =========================================================================
    // Variable evaluation tests (M2)
    // =========================================================================

    fn var_declaration(name: &str, value: IrValue) -> IrNode {
        IrNode::FunctionCall {
            name: "var".to_string(),
            positional_args: vec![IrValue::Identifier(name.to_string()), value],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 1),
        }
    }

    fn var_declaration_with_body(name: &str, body_nodes: Vec<IrNode>) -> IrNode {
        IrNode::FunctionCall {
            name: "var".to_string(),
            positional_args: vec![IrValue::Identifier(name.to_string())],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: Some(body_nodes),
            span: span(0, 1),
        }
    }

    fn var_ref(name: &str) -> IrNode {
        IrNode::FunctionCall {
            name: name.to_string(),
            positional_args: Vec::new(),
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 1),
        }
    }

    fn var_reassignment(name: &str, value: IrValue) -> IrNode {
        IrNode::FunctionCall {
            name: name.to_string(),
            positional_args: vec![value],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 1),
        }
    }

    fn inline_var_ref(name: &str) -> IrInline {
        IrInline::DirectiveCall {
            name: name.to_string(),
            positional_args: Vec::new(),
            named_args: Vec::new(),
            body: None,
            span: span(0, 1),
        }
    }

    fn evaluate_with_diagnostics(nodes: Vec<IrNode>) -> (Vec<IrNode>, Vec<Diagnostic>) {
        let (result, diagnostics) = Evaluator::new().evaluate(&doc(nodes));
        (result.nodes, diagnostics)
    }

    #[test]
    fn var_scalar_definition_and_reference() {
        let nodes = evaluate(vec![
            var_declaration("name", IrValue::String("Scribium".to_string())),
            var_ref("name"),
        ]);
        assert_eq!(nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "Scribium");
    }

    #[test]
    fn var_boolean_reference_in_conditional() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![
            var_declaration("enabled", IrValue::Identifier("yes".to_string())),
            IrNode::FunctionCall {
                name: "if".to_string(),
                positional_args: vec![IrValue::Identifier("enabled".to_string())],
                named_args: Vec::new(),
                lambda_parameters: None,
                body: Some(vec![text_paragraph("visible")]),
                span: span(0, 1),
            },
        ]);
        assert!(diagnostics.is_empty());
        assert_eq!(nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "visible");
    }

    #[test]
    fn var_false_boolean_drops_conditional() {
        let nodes = evaluate(vec![
            var_declaration("enabled", IrValue::Identifier("no".to_string())),
            IrNode::FunctionCall {
                name: "if".to_string(),
                positional_args: vec![IrValue::Identifier("enabled".to_string())],
                named_args: Vec::new(),
                lambda_parameters: None,
                body: Some(vec![text_paragraph("hidden")]),
                span: span(0, 1),
            },
        ]);
        assert!(nodes.is_empty());
    }

    #[test]
    fn var_ifnot_with_variable() {
        let nodes = evaluate(vec![
            var_declaration("enabled", IrValue::Identifier("no".to_string())),
            IrNode::FunctionCall {
                name: "ifnot".to_string(),
                positional_args: vec![IrValue::Identifier("enabled".to_string())],
                named_args: Vec::new(),
                lambda_parameters: None,
                body: Some(vec![text_paragraph("visible")]),
                span: span(0, 1),
            },
        ]);
        assert_eq!(nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "visible");
    }

    #[test]
    fn var_explicit_reassignment() {
        let nodes = evaluate(vec![
            var_declaration("name", IrValue::String("A".to_string())),
            var_declaration("name", IrValue::String("B".to_string())),
            var_ref("name"),
        ]);
        assert_eq!(nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "B");
    }

    #[test]
    fn var_variable_name_reassignment() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![
            var_declaration("name", IrValue::String("A".to_string())),
            var_ref("name"),
            var_reassignment("name", IrValue::String("B".to_string())),
            var_ref("name"),
        ]);
        assert!(diagnostics.is_empty());
        assert_eq!(nodes.len(), 2);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "A");
        let IrNode::Paragraph { content, .. } = &nodes[1] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "B");
    }

    #[test]
    fn var_reassignment_produces_no_output() {
        let nodes = evaluate(vec![
            var_declaration("name", IrValue::String("A".to_string())),
            var_reassignment("name", IrValue::String("B".to_string())),
        ]);
        assert!(nodes.is_empty());
    }

    #[test]
    fn var_inline_use() {
        let paragraph = IrNode::Paragraph {
            content: vec![
                text_inline("Hello "),
                inline_var_ref("name"),
                text_inline("!"),
            ],
            span: span(0, 1),
        };
        let nodes = evaluate(vec![
            var_declaration("name", IrValue::String("world".to_string())),
            paragraph,
        ]);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        let rendered: Vec<&str> = content
            .iter()
            .map(|i| match i {
                IrInline::Text { content, .. } => content.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(rendered, vec!["Hello ", "world", "!"]);
    }

    #[test]
    fn var_block_variable() {
        let body = vec![
            IrNode::Heading {
                level: 1,
                content: vec![text_inline("Title")],
                span: span(0, 1),
            },
            text_paragraph("body"),
        ];
        let nodes = evaluate(vec![
            var_declaration_with_body("section", body),
            var_ref("section"),
        ]);
        assert_eq!(nodes.len(), 2);
        let IrNode::Heading { content, .. } = &nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "Title");
        let IrNode::Paragraph { content, .. } = &nodes[1] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "body");
    }

    #[test]
    fn var_conditional_declaration_execution_order() {
        let (nodes, diagnostics) = evaluate_with_diagnostics(vec![
            IrNode::FunctionCall {
                name: "if".to_string(),
                positional_args: vec![IrValue::Boolean(false)],
                named_args: Vec::new(),
                lambda_parameters: None,
                body: Some(vec![var_declaration(
                    "x",
                    IrValue::String("hidden".to_string()),
                )]),
                span: span(0, 1),
            },
            var_ref("x"),
        ]);
        assert!(diagnostics.is_empty());
        // x should not be declared, so var_ref("x") is preserved as function call
        assert_eq!(nodes.len(), 1);
        let IrNode::FunctionCall { name, .. } = &nodes[0] else {
            panic!()
        };
        assert_eq!(name, "x");
    }

    #[test]
    fn var_unknown_call_preserved() {
        let nodes = evaluate(vec![var_ref("unknown")]);
        assert_eq!(nodes.len(), 1);
        let IrNode::FunctionCall { name, .. } = &nodes[0] else {
            panic!()
        };
        assert_eq!(name, "unknown");
    }

    #[test]
    fn var_malformed_declaration_reports_e3002() {
        let call = IrNode::FunctionCall {
            name: "var".to_string(),
            positional_args: Vec::new(),
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(3, 6),
        };
        let (result, diagnostics) = Evaluator::new().evaluate(&doc(vec![call]));
        assert!(result.nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3002");
        assert!(matches!(diagnostics[0].severity, Severity::Error));
        assert_eq!(diagnostics[0].primary, Some(span(3, 6)));
    }

    #[test]
    fn var_nested_evaluation_in_block_variable() {
        let body = vec![IrNode::FunctionCall {
            name: "if".to_string(),
            positional_args: vec![IrValue::Boolean(true)],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: Some(vec![text_paragraph("nested visible")]),
            span: span(0, 1),
        }];
        let nodes = evaluate(vec![
            var_declaration_with_body("section", body),
            var_ref("section"),
        ]);
        assert_eq!(nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "nested visible");
    }

    #[test]
    fn var_evaluation_immutable_and_deterministic() {
        let input = doc(vec![
            var_declaration("name", IrValue::String("A".to_string())),
            var_ref("name"),
        ]);
        let first = Evaluator::new().evaluate(&input);
        assert_eq!(input.nodes.len(), 2);
        let second = Evaluator::new().evaluate(&input);
        assert_eq!(first.0, second.0);
        assert!(first.1.is_empty() && second.1.is_empty());
    }

    #[test]
    fn var_content_value_block_reference() {
        // .var {x} {**hello**} should preserve the strong content
        let strong_hello = IrNode::Paragraph {
            content: vec![IrInline::Strong {
                content: vec![IrInline::Text {
                    content: "hello".to_string(),
                    span: span(0, 5),
                }],
                span: span(0, 5),
            }],
            span: span(0, 11),
        };
        let nodes = evaluate(vec![
            var_declaration("x", IrValue::Content(vec![strong_hello.clone()])),
            var_ref("x"),
        ]);
        assert_eq!(nodes.len(), 1);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        let IrInline::Strong {
            content: strong_content,
            ..
        } = &content[0]
        else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &strong_content[0] else {
            panic!()
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn var_content_value_inline_reference() {
        // .var {x} {**world**} / Hello .x should preserve strong in inline context
        let strong_world = IrInline::Strong {
            content: vec![IrInline::Text {
                content: "world".to_string(),
                span: span(0, 5),
            }],
            span: span(0, 5),
        };
        let paragraph = IrNode::Paragraph {
            content: vec![
                IrInline::Text {
                    content: "Hello ".to_string(),
                    span: span(0, 6),
                },
                inline_var_ref("x"),
            ],
            span: span(0, 1),
        };
        let nodes = evaluate(vec![
            var_declaration(
                "x",
                IrValue::Content(vec![IrNode::Paragraph {
                    content: vec![strong_world.clone()],
                    span: span(0, 1),
                }]),
            ),
            paragraph,
        ]);
        let IrNode::Paragraph { content, .. } = &nodes[0] else {
            panic!()
        };
        assert_eq!(content.len(), 2);
        let IrInline::Text { content: text, .. } = &content[0] else {
            panic!()
        };
        assert_eq!(text, "Hello ");
        let IrInline::Strong {
            content: strong_content,
            ..
        } = &content[1]
        else {
            panic!()
        };
        let IrInline::Text { content: text, .. } = &strong_content[0] else {
            panic!()
        };
        assert_eq!(text, "world");
    }

    #[test]
    fn var_reference_with_body_is_not_variable_reference() {
        // .var {foo} {value} / .foo { body } should preserve the call with body
        let body = vec![text_paragraph("body")];
        let nodes = evaluate(vec![
            var_declaration("foo", IrValue::String("value".to_string())),
            IrNode::FunctionCall {
                name: "foo".to_string(),
                positional_args: Vec::new(),
                named_args: Vec::new(),
                lambda_parameters: None,
                body: Some(body),
                span: span(0, 1),
            },
        ]);
        // Should be preserved as function call, not variable reference
        assert_eq!(nodes.len(), 1);
        let IrNode::FunctionCall {
            name,
            body: call_body,
            ..
        } = &nodes[0]
        else {
            panic!()
        };
        assert_eq!(name, "foo");
        assert!(call_body.is_some());
        assert_eq!(call_body.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn var_invalid_name_reports_e3002() {
        // .var {"bad name"} {hello} should report E3002
        let call = IrNode::FunctionCall {
            name: "var".to_string(),
            positional_args: vec![
                IrValue::String("bad name".to_string()),
                IrValue::String("hello".to_string()),
            ],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 25),
        };
        let (result, diagnostics) = Evaluator::new().evaluate(&doc(vec![call]));
        assert!(result.nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3002");
        assert!(matches!(diagnostics[0].severity, Severity::Error));
        assert!(diagnostics[0].message.contains("Invalid variable name"));
    }

    #[test]
    fn var_empty_name_reports_e3002() {
        // .var {""} {hello} should report E3002
        let call = IrNode::FunctionCall {
            name: "var".to_string(),
            positional_args: vec![
                IrValue::String("".to_string()),
                IrValue::String("hello".to_string()),
            ],
            named_args: Vec::new(),
            lambda_parameters: None,
            body: None,
            span: span(0, 17),
        };
        let (result, diagnostics) = Evaluator::new().evaluate(&doc(vec![call]));
        assert!(result.nodes.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "E3002");
        assert!(diagnostics[0].message.contains("Invalid variable name"));
    }

    #[test]
    fn caller_overlay_failure_does_not_mutate_capture_or_caller_context() {
        let capture = IrCallableCapture {
            variables: vec![IrCapturedVariable {
                name: "value".to_string(),
                value: IrValue::String("definition".to_string()),
            }],
            functions: Vec::new(),
        };
        let callable = IrCallable {
            parameters: None,
            body: vec![IrNode::FunctionCall {
                name: "multiply".to_string(),
                positional_args: vec![IrValue::Boolean(true), IrValue::Number(2.0)],
                named_args: Vec::new(),
                lambda_parameters: None,
                body: None,
                span: span(10, 20),
            }],
            span: span(0, 20),
            capture: Some(Box::new(capture)),
        };
        let original_capture = callable.capture.clone();
        let mut caller_context = EvaluationContext::new();
        caller_context.set_value("value".to_string(), IrValue::String("caller".to_string()));
        let mut diagnostics = Vec::new();

        let outcome = Evaluator::new().invoke_callable(
            &callable,
            Vec::new(),
            IterationOptions {
                span: span(0, 20),
                allow_destructuring: false,
            },
            &mut diagnostics,
            &caller_context,
        );

        assert!(matches!(outcome, CallOutcome::Failed));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(callable.capture, original_capture);
        assert_eq!(
            caller_context.get("value").map(VariableValue::to_value),
            Some(IrValue::String("caller".to_string()))
        );
    }
}
