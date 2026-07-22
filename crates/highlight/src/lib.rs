//! Rust-side Tree-sitter syntax highlighting for immutable review snapshots.
//!
//! The crate intentionally returns byte spans rather than HTML. That keeps the
//! renderer in charge of presentation while keeping parsing out of Svelte and
//! preserving row geometry during asynchronous token updates.

use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use localreview_domain::DiffSide;
use serde::{Deserialize, Serialize};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Pinned grammar/query bundle. Bump this whenever any grammar/query changes
/// so old cache entries cannot be reused against different token semantics.
pub const GRAMMAR_BUNDLE_VERSION: &str = "tree-sitter-2026-07-22-v2";

const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "boolean",
    "comment",
    "constant",
    "constructor",
    "embedded",
    "escape",
    "function",
    "function.builtin",
    "keyword",
    "markup",
    "markup.heading",
    "markup.link",
    "markup.raw",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "string",
    "string.escape",
    "tag",
    "type",
    "variable",
    "variable.builtin",
    "variable.member",
    "variable.parameter",
];

/// A language known to LocalReview's resolver. `Svelte` deliberately has a
/// graceful plain-text fallback until a pinned mixed-language grammar is added.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HighlightLanguage {
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
    Json,
    Python,
    Markdown,
    Shell,
    Swift,
    Starlark,
    Toml,
    Yaml,
    Go,
    Java,
    C,
    Cpp,
    Html,
    Css,
    Svelte,
}

impl HighlightLanguage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Json => "json",
            Self::Python => "python",
            Self::Markdown => "markdown",
            Self::Shell => "shell",
            Self::Swift => "swift",
            Self::Starlark => "starlark",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
            Self::Go => "go",
            Self::Java => "java",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Html => "html",
            Self::Css => "css",
            Self::Svelte => "svelte",
        }
    }
}

/// A theme participates in the cache key even though semantic token classes
/// are theme-neutral. A renderer may safely map these classes to a different
/// palette without applying stale theme-specific work later.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HighlightTheme {
    #[default]
    Dark,
    Light,
    Custom(String),
}

/// Stable token category understood by the UI theme map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntaxClass {
    Attribute,
    Boolean,
    Comment,
    Constant,
    Constructor,
    Embedded,
    Escape,
    Function,
    Keyword,
    Markup,
    Module,
    Number,
    Operator,
    Property,
    Punctuation,
    String,
    Tag,
    Type,
    Variable,
}

impl SyntaxClass {
    fn from_capture(name: &str) -> Self {
        if name.starts_with("comment") {
            Self::Comment
        } else if name.starts_with("string") {
            Self::String
        } else if name.starts_with("function") {
            Self::Function
        } else if name.starts_with("keyword") {
            Self::Keyword
        } else if name.starts_with("type") {
            Self::Type
        } else if name.starts_with("variable") {
            Self::Variable
        } else if name.starts_with("constant") {
            Self::Constant
        } else if name.starts_with("number") {
            Self::Number
        } else if name.starts_with("boolean") {
            Self::Boolean
        } else if name.starts_with("operator") {
            Self::Operator
        } else if name.starts_with("property") {
            Self::Property
        } else if name.starts_with("punctuation") {
            Self::Punctuation
        } else if name.starts_with("constructor") {
            Self::Constructor
        } else if name.starts_with("attribute") {
            Self::Attribute
        } else if name.starts_with("embedded") {
            Self::Embedded
        } else if name.starts_with("escape") {
            Self::Escape
        } else if name.starts_with("markup") {
            Self::Markup
        } else if name.starts_with("module") {
            Self::Module
        } else if name.starts_with("tag") {
            Self::Tag
        } else {
            Self::Variable
        }
    }
}

/// A validated UTF-8 byte span into the complete source document. Spans are
/// side-aware and never refer to virtualized row indexes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSpan {
    pub side: DiffSide,
    pub start_byte: u32,
    pub end_byte: u32,
    pub class: SyntaxClass,
}

/// Cache identity for a complete old/new source document.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HighlightCacheKey {
    pub source_fingerprint: String,
    pub side: DiffSide,
    pub language: HighlightLanguage,
    pub grammar_bundle_version: String,
    pub theme: HighlightTheme,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlainTextReason {
    UnknownLanguage,
    UnsupportedMixedLanguage,
    Binary,
    Generated,
    FileTooLarge,
    TooManyLines,
    ParseFailure,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "reason")]
pub enum HighlightStatus {
    Highlighted,
    PlainText(PlainTextReason),
}

/// A compact, parser-derived navigation entry.  This intentionally contains
/// no source text: callers keep the immutable snapshot authoritative and use
/// the byte/line boundaries only for navigation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineSymbol {
    pub name: String,
    pub kind: OutlineKind,
    pub start_line: u32,
    pub end_line: u32,
    pub depth: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutlineKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Module,
    Heading,
    Property,
    Unknown,
}

/// A complete response. An empty `tokens` vector is valid and renders the same
/// plain monospaced rows immediately available before the job began.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightResult {
    pub key: Option<HighlightCacheKey>,
    pub language: Option<HighlightLanguage>,
    pub status: HighlightStatus,
    pub tokens: Vec<TokenSpan>,
}

#[derive(Clone, Debug)]
pub struct HighlightRequest<'a> {
    pub path: &'a Path,
    pub source: &'a str,
    pub side: DiffSide,
    pub language_attribute: Option<&'a str>,
    pub theme: HighlightTheme,
    /// Allows an explicit user action to bypass generated/large-file policy.
    pub force: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightPolicy {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub highlight_generated: bool,
}

impl Default for HighlightPolicy {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024,
            max_lines: 10_000,
            highlight_generated: false,
        }
    }
}

/// Cancellation is cooperative and can be shared with the service job
/// scheduler. Tree-sitter receives the same atomic directly.
#[derive(Clone, Debug, Default)]
pub struct HighlightCancellation(Arc<AtomicUsize>);

impl HighlightCancellation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(1, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) != 0
    }
}

/// A bounded weighted LRU cache. Cache weight is source bytes plus compact
/// token metadata, rather than entry count, to prevent a few huge files from
/// quietly retaining unbounded memory.
#[derive(Clone, Debug)]
pub struct HighlightCacheConfig {
    pub max_weight_bytes: usize,
}

impl Default for HighlightCacheConfig {
    fn default() -> Self {
        Self {
            max_weight_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HighlightService {
    policy: HighlightPolicy,
    cache: Arc<Mutex<WeightedLru>>,
}

impl HighlightService {
    #[must_use]
    pub fn new(policy: HighlightPolicy, cache: HighlightCacheConfig) -> Self {
        Self {
            policy,
            cache: Arc::new(Mutex::new(WeightedLru::new(cache.max_weight_bytes))),
        }
    }

    /// Returns the no-work plain-text presentation used by a caller while it
    /// schedules `highlight` on a background worker.
    #[must_use]
    pub fn plain_presentation(&self, request: &HighlightRequest<'_>) -> HighlightResult {
        let language = resolve_language(request.path, request.source, request.language_attribute);
        let reason = self.eligibility_reason(request, language);
        HighlightResult {
            key: language.map(|language| cache_key(request, language)),
            language,
            status: reason.map_or(HighlightStatus::Highlighted, HighlightStatus::PlainText),
            tokens: Vec::new(),
        }
    }

    /// Highlights an entire immutable source document. Call this outside the
    /// UI thread; result byte ranges never alter line/row geometry.
    #[must_use]
    pub fn highlight(
        &self,
        request: &HighlightRequest<'_>,
        cancellation: Option<&HighlightCancellation>,
    ) -> HighlightResult {
        let language = resolve_language(request.path, request.source, request.language_attribute);
        let Some(language) = language else {
            return plain(None, PlainTextReason::UnknownLanguage);
        };
        if let Some(reason) = self.eligibility_reason(request, Some(language)) {
            return plain(Some(language), reason);
        }
        if matches!(language, HighlightLanguage::Svelte) {
            return plain(Some(language), PlainTextReason::UnsupportedMixedLanguage);
        }
        let key = cache_key(request, language);
        if let Ok(mut cache) = self.cache.lock() {
            if let Some(tokens) = cache.get(&key) {
                return HighlightResult {
                    key: Some(key),
                    language: Some(language),
                    status: HighlightStatus::Highlighted,
                    tokens,
                };
            }
        }
        if cancellation.is_some_and(HighlightCancellation::is_cancelled) {
            return plain(Some(language), PlainTextReason::Cancelled);
        }
        let config = match configuration(language) {
            Ok(Some(config)) => config,
            Ok(None) => return plain(Some(language), PlainTextReason::UnknownLanguage),
            Err(_) => return plain(Some(language), PlainTextReason::ParseFailure),
        };
        let mut highlighter = Highlighter::new();
        let raw_cancellation = cancellation.map(|value| value.0.as_ref());
        let events =
            match highlighter.highlight(config, request.source.as_bytes(), raw_cancellation, |_| {
                None
            }) {
                Ok(events) => events,
                Err(tree_sitter_highlight::Error::Cancelled) => {
                    return plain(Some(language), PlainTextReason::Cancelled);
                }
                Err(_) => return plain(Some(language), PlainTextReason::ParseFailure),
            };
        let mut stack = Vec::new();
        let mut tokens = Vec::new();
        for event in events {
            match event {
                Ok(HighlightEvent::HighlightStart(highlight)) => stack.push(highlight.0),
                Ok(HighlightEvent::HighlightEnd) => {
                    stack.pop();
                }
                Ok(HighlightEvent::Source { start, end }) => {
                    let Some(index) = stack.last().copied() else {
                        continue;
                    };
                    let Some(name) = HIGHLIGHT_NAMES.get(index) else {
                        continue;
                    };
                    push_valid_span(
                        &mut tokens,
                        request.source,
                        request.side,
                        start,
                        end,
                        SyntaxClass::from_capture(name),
                    );
                }
                Err(tree_sitter_highlight::Error::Cancelled) => {
                    return plain(Some(language), PlainTextReason::Cancelled);
                }
                Err(_) => return plain(Some(language), PlainTextReason::ParseFailure),
            }
        }
        let weight = request
            .source
            .len()
            .saturating_add(tokens.len().saturating_mul(24));
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(key.clone(), tokens.clone(), weight);
        }
        HighlightResult {
            key: Some(key),
            language: Some(language),
            status: HighlightStatus::Highlighted,
            tokens,
        }
    }

    #[must_use]
    pub fn cache_weight_bytes(&self) -> usize {
        self.cache.lock().map_or(0, |cache| cache.weight)
    }

    fn eligibility_reason(
        &self,
        request: &HighlightRequest<'_>,
        language: Option<HighlightLanguage>,
    ) -> Option<PlainTextReason> {
        if request.source.as_bytes().contains(&0) {
            return Some(PlainTextReason::Binary);
        }
        if !request.force
            && !self.policy.highlight_generated
            && is_generated(request.path, request.source)
        {
            return Some(PlainTextReason::Generated);
        }
        if !request.force && request.source.len() > self.policy.max_bytes {
            return Some(PlainTextReason::FileTooLarge);
        }
        if !request.force && line_count(request.source) > self.policy.max_lines {
            return Some(PlainTextReason::TooManyLines);
        }
        if matches!(language, Some(HighlightLanguage::Svelte)) {
            return None;
        }
        None
    }
}

impl Default for HighlightService {
    fn default() -> Self {
        Self::new(HighlightPolicy::default(), HighlightCacheConfig::default())
    }
}

fn plain(language: Option<HighlightLanguage>, reason: PlainTextReason) -> HighlightResult {
    HighlightResult {
        key: None,
        language,
        status: HighlightStatus::PlainText(reason),
        tokens: Vec::new(),
    }
}

fn cache_key(request: &HighlightRequest<'_>, language: HighlightLanguage) -> HighlightCacheKey {
    HighlightCacheKey {
        source_fingerprint: blake3::hash(request.source.as_bytes()).to_hex().to_string(),
        side: request.side,
        language,
        grammar_bundle_version: GRAMMAR_BUNDLE_VERSION.to_owned(),
        theme: request.theme.clone(),
    }
}

fn line_count(source: &str) -> usize {
    source.lines().count()
}

fn is_generated(path: &Path, source: &str) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if name.ends_with(".min.js") || name.ends_with(".min.css") || name.ends_with(".map") {
        return true;
    }
    source.lines().take(5).any(|line| {
        let lowered = line.to_ascii_lowercase();
        lowered.contains("@generated")
            || lowered.contains("code generated")
            || lowered.contains("generated by") && lowered.contains("do not edit")
    })
}

/// Produces a bounded code outline from the same pinned Tree-sitter grammars
/// used for highlighting. Unsupported or mixed-language files deliberately
/// return an empty list instead of a heuristic outline that could navigate to
/// the wrong immutable line.
#[must_use]
pub fn outline(path: &Path, source: &str, language_attribute: Option<&str>) -> Vec<OutlineSymbol> {
    const MAX_SYMBOLS: usize = 1_000;
    let Some(language) = resolve_language(path, source, language_attribute) else {
        return Vec::new();
    };
    let Some(grammar) = parser_language(language) else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut symbols = Vec::new();
    collect_outline(
        tree.root_node(),
        source.as_bytes(),
        language,
        0,
        &mut symbols,
        MAX_SYMBOLS,
    );
    symbols
}

fn parser_language(language: HighlightLanguage) -> Option<tree_sitter::Language> {
    Some(match language {
        HighlightLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        HighlightLanguage::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        HighlightLanguage::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        HighlightLanguage::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        HighlightLanguage::Json => tree_sitter_json::LANGUAGE.into(),
        HighlightLanguage::Python => tree_sitter_python::LANGUAGE.into(),
        HighlightLanguage::Markdown => tree_sitter_md::LANGUAGE.into(),
        HighlightLanguage::Shell => tree_sitter_bash::LANGUAGE.into(),
        HighlightLanguage::Swift => tree_sitter_swift::LANGUAGE.into(),
        HighlightLanguage::Starlark => tree_sitter_starlark::LANGUAGE.into(),
        HighlightLanguage::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
        HighlightLanguage::Yaml => tree_sitter_yaml::LANGUAGE.into(),
        HighlightLanguage::Go => tree_sitter_go::LANGUAGE.into(),
        HighlightLanguage::Java => tree_sitter_java::LANGUAGE.into(),
        HighlightLanguage::C => tree_sitter_c::LANGUAGE.into(),
        HighlightLanguage::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        HighlightLanguage::Html => tree_sitter_html::LANGUAGE.into(),
        HighlightLanguage::Css => tree_sitter_css::LANGUAGE.into(),
        // Svelte needs a mixed-language parser/query bundle, which we do not
        // pretend to have. Returning no entries is safe and predictable.
        HighlightLanguage::Svelte => return None,
    })
}

fn collect_outline(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    language: HighlightLanguage,
    depth: u16,
    symbols: &mut Vec<OutlineSymbol>,
    max_symbols: usize,
) {
    if symbols.len() >= max_symbols {
        return;
    }
    let symbol_kind = outline_kind(language, node.kind());
    let child_depth = if let Some(kind) = symbol_kind {
        let name = outline_name(node, source).unwrap_or_else(|| node.kind().replace('_', " "));
        let start_line =
            u32::try_from(node.start_position().row.saturating_add(1)).unwrap_or(u32::MAX);
        let end_line = u32::try_from(node.end_position().row.saturating_add(1)).unwrap_or(u32::MAX);
        symbols.push(OutlineSymbol {
            name,
            kind,
            start_line,
            end_line: end_line.max(start_line),
            depth,
        });
        depth.saturating_add(1)
    } else {
        depth
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_outline(child, source, language, child_depth, symbols, max_symbols);
        if symbols.len() >= max_symbols {
            break;
        }
    }
}

fn outline_kind(language: HighlightLanguage, kind: &str) -> Option<OutlineKind> {
    let kind = match kind {
        "function_item" | "function_declaration" | "function_definition" | "arrow_function" => {
            OutlineKind::Function
        }
        "method_definition" | "method_declaration" => OutlineKind::Method,
        "class_declaration" | "class_definition" | "class_specifier" => OutlineKind::Class,
        "struct_item" | "struct_specifier" => OutlineKind::Struct,
        "enum_item" | "enum_declaration" | "enum_specifier" => OutlineKind::Enum,
        "interface_declaration" | "trait_item" => OutlineKind::Interface,
        "mod_item" | "module" | "module_declaration" | "namespace_declaration" => {
            OutlineKind::Module
        }
        "atx_heading" | "setext_heading" => OutlineKind::Heading,
        "field_declaration"
        | "property_declaration"
        | "variable_declarator"
        | "const_item"
        | "static_item" => OutlineKind::Property,
        _ => return None,
    };
    // JSON/YAML/TOML nodes can be vast and are not reliable code navigation;
    // retain headings and declarations only for syntaxes with useful symbols.
    if matches!(
        language,
        HighlightLanguage::Json | HighlightLanguage::Yaml | HighlightLanguage::Toml
    ) && !matches!(kind, OutlineKind::Heading)
    {
        None
    } else {
        Some(kind)
    }
}

fn outline_name(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    for field in ["name", "declarator", "type", "property", "key"] {
        if let Some(value) = node.child_by_field_name(field) {
            if let Ok(text) = value.utf8_text(source) {
                let text = text.trim();
                if !text.is_empty() {
                    return Some(text.chars().take(160).collect());
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "identifier" | "type_identifier" | "property_identifier" | "heading_content"
        ) {
            if let Ok(text) = child.utf8_text(source) {
                let text = text.trim();
                if !text.is_empty() {
                    return Some(text.chars().take(160).collect());
                }
            }
        }
    }
    None
}

/// Language resolution is deterministic: explicit attributes win, followed by
/// exact special filenames, extension, then a conventional shebang. Filename
/// matching intentionally uses only the final component so a directory named
/// `BUILD` or `Cargo.toml` cannot affect a source file below it.
#[must_use]
pub fn resolve_language(
    path: &Path,
    source: &str,
    language_attribute: Option<&str>,
) -> Option<HighlightLanguage> {
    if let Some(language) = language_attribute.and_then(parse_language_name) {
        return Some(language);
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let exact = match name {
        "Cargo.toml" | "pyproject.toml" => Some(HighlightLanguage::Toml),
        "BUILD" | "BUILD.bazel" | "WORKSPACE" | "WORKSPACE.bazel" => {
            Some(HighlightLanguage::Starlark)
        }
        "MODULE.bazel" => Some(HighlightLanguage::Starlark),
        "Makefile" | "makefile" | "GNUmakefile" => Some(HighlightLanguage::Shell),
        _ => None,
    };
    if exact.is_some() {
        return exact;
    }
    let by_extension =
        path.extension()
            .and_then(|extension| extension.to_str())
            .and_then(|extension| match extension.to_ascii_lowercase().as_str() {
                "rs" => Some(HighlightLanguage::Rust),
                "js" | "mjs" | "cjs" | "jsx" => Some(HighlightLanguage::JavaScript),
                "ts" | "mts" | "cts" => Some(HighlightLanguage::TypeScript),
                "tsx" => Some(HighlightLanguage::Tsx),
                "json" | "jsonc" | "json5" => Some(HighlightLanguage::Json),
                "py" | "pyi" | "pyw" => Some(HighlightLanguage::Python),
                "md" | "mdx" | "markdown" => Some(HighlightLanguage::Markdown),
                "sh" | "bash" | "zsh" | "fish" => Some(HighlightLanguage::Shell),
                "swift" => Some(HighlightLanguage::Swift),
                "bzl" | "bazel" | "star" | "starlark" => Some(HighlightLanguage::Starlark),
                "toml" => Some(HighlightLanguage::Toml),
                "yaml" | "yml" => Some(HighlightLanguage::Yaml),
                "go" => Some(HighlightLanguage::Go),
                "java" => Some(HighlightLanguage::Java),
                "c" | "h" => Some(HighlightLanguage::C),
                "cc" | "cp" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp"
                | "inl" => Some(HighlightLanguage::Cpp),
                "html" | "htm" | "xhtml" | "svg" => Some(HighlightLanguage::Html),
                "css" => Some(HighlightLanguage::Css),
                "svelte" => Some(HighlightLanguage::Svelte),
                _ => None,
            });
    by_extension.or_else(|| language_from_shebang(source))
}

fn parse_language_name(value: &str) -> Option<HighlightLanguage> {
    match value.trim().to_ascii_lowercase().as_str() {
        "rust" => Some(HighlightLanguage::Rust),
        "javascript" | "js" | "jsx" => Some(HighlightLanguage::JavaScript),
        "typescript" | "ts" => Some(HighlightLanguage::TypeScript),
        "tsx" => Some(HighlightLanguage::Tsx),
        "json" => Some(HighlightLanguage::Json),
        "python" | "py" => Some(HighlightLanguage::Python),
        "markdown" | "md" => Some(HighlightLanguage::Markdown),
        "shell" | "sh" | "bash" | "zsh" => Some(HighlightLanguage::Shell),
        "swift" => Some(HighlightLanguage::Swift),
        "starlark" | "bazel" | "bzl" => Some(HighlightLanguage::Starlark),
        "toml" => Some(HighlightLanguage::Toml),
        "yaml" | "yml" => Some(HighlightLanguage::Yaml),
        "go" | "golang" => Some(HighlightLanguage::Go),
        "java" => Some(HighlightLanguage::Java),
        "c" => Some(HighlightLanguage::C),
        "cpp" | "c++" | "cxx" => Some(HighlightLanguage::Cpp),
        "html" | "xhtml" => Some(HighlightLanguage::Html),
        "css" => Some(HighlightLanguage::Css),
        "svelte" => Some(HighlightLanguage::Svelte),
        _ => None,
    }
}

fn language_from_shebang(source: &str) -> Option<HighlightLanguage> {
    let first = source.lines().next()?.trim_start_matches('\u{feff}');
    let command = first.strip_prefix("#!")?.trim();
    let mut words = command.split_ascii_whitespace();
    let program = words.next()?;
    let program = Path::new(program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(program);
    let interpreter = if program.eq_ignore_ascii_case("env") {
        // `env` accepts flags such as `-i`, `--ignore-environment`, and `-S`.
        // The first non-flag word is the executable regardless of whether
        // `-S` is used to split a multi-word command string.
        words.find(|word| !word.starts_with('-'))?
    } else {
        program
    };
    let interpreter = Path::new(interpreter)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(interpreter)
        .to_ascii_lowercase();
    if interpreter.starts_with("python") {
        Some(HighlightLanguage::Python)
    } else if matches!(interpreter.as_str(), "node" | "nodejs" | "deno" | "bun") {
        Some(HighlightLanguage::JavaScript)
    } else if matches!(
        interpreter.as_str(),
        "bash" | "zsh" | "sh" | "dash" | "ksh" | "fish"
    ) {
        Some(HighlightLanguage::Shell)
    } else if interpreter == "swift" {
        Some(HighlightLanguage::Swift)
    } else {
        None
    }
}

fn push_valid_span(
    tokens: &mut Vec<TokenSpan>,
    source: &str,
    side: DiffSide,
    start: usize,
    end: usize,
    class: SyntaxClass,
) {
    if start >= end
        || end > source.len()
        || !source.is_char_boundary(start)
        || !source.is_char_boundary(end)
    {
        return;
    }
    let (Ok(start_byte), Ok(end_byte)) = (u32::try_from(start), u32::try_from(end)) else {
        return;
    };
    if let Some(previous) = tokens.last_mut() {
        if previous.side == side && previous.class == class && previous.end_byte == start_byte {
            previous.end_byte = end_byte;
            return;
        }
    }
    tokens.push(TokenSpan {
        side,
        start_byte,
        end_byte,
        class,
    });
}

fn configuration(
    language: HighlightLanguage,
) -> Result<Option<&'static HighlightConfiguration>, &'static str> {
    match language {
        HighlightLanguage::Rust => rust_config().map(Some),
        HighlightLanguage::JavaScript => javascript_config().map(Some),
        HighlightLanguage::TypeScript => typescript_config().map(Some),
        HighlightLanguage::Tsx => tsx_config().map(Some),
        HighlightLanguage::Json => json_config().map(Some),
        HighlightLanguage::Python => python_config().map(Some),
        HighlightLanguage::Markdown => markdown_config().map(Some),
        HighlightLanguage::Shell => shell_config().map(Some),
        HighlightLanguage::Swift => swift_config().map(Some),
        HighlightLanguage::Starlark => starlark_config().map(Some),
        HighlightLanguage::Toml => toml_config().map(Some),
        HighlightLanguage::Yaml => yaml_config().map(Some),
        HighlightLanguage::Go => go_config().map(Some),
        HighlightLanguage::Java => java_config().map(Some),
        HighlightLanguage::C => c_config().map(Some),
        HighlightLanguage::Cpp => cpp_config().map(Some),
        HighlightLanguage::Html => html_config().map(Some),
        HighlightLanguage::Css => css_config().map(Some),
        HighlightLanguage::Svelte => Ok(None),
    }
}

fn configured(
    language: tree_sitter::Language,
    name: &'static str,
    highlights: &'static str,
) -> Result<HighlightConfiguration, tree_sitter::QueryError> {
    let mut config = HighlightConfiguration::new(language, name, highlights, "", "")?;
    config.configure(HIGHLIGHT_NAMES);
    Ok(config)
}

fn rust_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_rust::LANGUAGE.into(),
                "rust",
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn javascript_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_javascript::LANGUAGE.into(),
                "javascript",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn typescript_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                "typescript",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn tsx_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                "tsx",
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn json_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_json::LANGUAGE.into(),
                "json",
                tree_sitter_json::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn python_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_python::LANGUAGE.into(),
                "python",
                tree_sitter_python::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn markdown_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_md::LANGUAGE.into(),
                "markdown",
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn shell_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_bash::LANGUAGE.into(),
                "shell",
                tree_sitter_bash::HIGHLIGHT_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn swift_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_swift::LANGUAGE.into(),
                "swift",
                tree_sitter_swift::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn starlark_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_starlark::LANGUAGE.into(),
                "starlark",
                tree_sitter_starlark::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn toml_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_toml_ng::LANGUAGE.into(),
                "toml",
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn yaml_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_yaml::LANGUAGE.into(),
                "yaml",
                tree_sitter_yaml::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn go_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_go::LANGUAGE.into(),
                "go",
                tree_sitter_go::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn java_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_java::LANGUAGE.into(),
                "java",
                tree_sitter_java::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn c_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_c::LANGUAGE.into(),
                "c",
                tree_sitter_c::HIGHLIGHT_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn cpp_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_cpp::LANGUAGE.into(),
                "cpp",
                tree_sitter_cpp::HIGHLIGHT_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn html_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_html::LANGUAGE.into(),
                "html",
                tree_sitter_html::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

fn css_config() -> Result<&'static HighlightConfiguration, &'static str> {
    static CONFIG: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            configured(
                tree_sitter_css::LANGUAGE.into(),
                "css",
                tree_sitter_css::HIGHLIGHTS_QUERY,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

#[derive(Debug)]
struct CacheEntry {
    tokens: Vec<TokenSpan>,
    weight: usize,
}

#[derive(Debug)]
struct WeightedLru {
    entries: HashMap<HighlightCacheKey, CacheEntry>,
    recency: VecDeque<HighlightCacheKey>,
    capacity: usize,
    weight: usize,
}

impl WeightedLru {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            recency: VecDeque::new(),
            capacity,
            weight: 0,
        }
    }

    fn get(&mut self, key: &HighlightCacheKey) -> Option<Vec<TokenSpan>> {
        let tokens = self.entries.get(key)?.tokens.clone();
        self.touch(key);
        Some(tokens)
    }

    fn insert(&mut self, key: HighlightCacheKey, tokens: Vec<TokenSpan>, weight: usize) {
        if self.capacity == 0 || weight > self.capacity {
            return;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.weight = self.weight.saturating_sub(previous.weight);
            self.recency.retain(|candidate| candidate != &key);
        }
        self.weight = self.weight.saturating_add(weight);
        self.recency.push_back(key.clone());
        self.entries.insert(key, CacheEntry { tokens, weight });
        while self.weight > self.capacity {
            let Some(oldest) = self.recency.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.weight = self.weight.saturating_sub(entry.weight);
            }
        }
    }

    fn touch(&mut self, key: &HighlightCacheKey) {
        self.recency.retain(|candidate| candidate != key);
        self.recency.push_back(key.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn request<'a>(path: &'a str, source: &'a str, side: DiffSide) -> HighlightRequest<'a> {
        HighlightRequest {
            path: Path::new(path),
            source,
            side,
            language_attribute: None,
            theme: HighlightTheme::Dark,
            force: false,
        }
    }

    #[test]
    fn resolves_extensions_attributes_and_shebangs() {
        assert_eq!(
            resolve_language(Path::new("code.rs"), "", None),
            Some(HighlightLanguage::Rust)
        );
        assert_eq!(
            resolve_language(Path::new("script"), "#!/usr/bin/env python3\n", None),
            Some(HighlightLanguage::Python)
        );
        assert_eq!(
            resolve_language(Path::new("custom"), "", Some("typescript")),
            Some(HighlightLanguage::TypeScript)
        );
        assert_eq!(
            resolve_language(Path::new("BUILD"), "", Some("rust")),
            Some(HighlightLanguage::Rust),
            "an explicit diff language must win over a special filename"
        );
        assert_eq!(
            resolve_language(Path::new("view.svelte"), "", None),
            Some(HighlightLanguage::Svelte)
        );
    }

    #[test]
    fn resolves_complete_initial_language_set_and_special_filenames() {
        let resolutions = [
            ("Sources/App.swift", HighlightLanguage::Swift),
            ("BUILD", HighlightLanguage::Starlark),
            ("BUILD.bazel", HighlightLanguage::Starlark),
            ("MODULE.bazel", HighlightLanguage::Starlark),
            ("rules/example.bzl", HighlightLanguage::Starlark),
            ("Cargo.toml", HighlightLanguage::Toml),
            ("config/settings.YML", HighlightLanguage::Yaml),
            ("cmd/main.go", HighlightLanguage::Go),
            ("src/Main.java", HighlightLanguage::Java),
            ("native/value.c", HighlightLanguage::C),
            ("include/value.H", HighlightLanguage::C),
            ("native/value.cxx", HighlightLanguage::Cpp),
            ("include/value.hpp", HighlightLanguage::Cpp),
            ("web/page.XHTML", HighlightLanguage::Html),
            ("web/icon.svg", HighlightLanguage::Html),
            ("web/theme.CSS", HighlightLanguage::Css),
        ];
        for (path, expected) in resolutions {
            assert_eq!(
                resolve_language(Path::new(path), "", None),
                Some(expected),
                "{path}"
            );
        }
        assert_eq!(
            resolve_language(
                Path::new("script"),
                "\u{feff}#!/usr/bin/env -S swift --quiet\nprint(\"ok\")\n",
                None,
            ),
            Some(HighlightLanguage::Swift)
        );
        assert_eq!(
            resolve_language(
                Path::new("script"),
                "#!/usr/bin/env --ignore-environment python3\n",
                None,
            ),
            Some(HighlightLanguage::Python)
        );
        assert_eq!(
            resolve_language(Path::new("script"), "#!/usr/bin/env node\n", None,),
            Some(HighlightLanguage::JavaScript)
        );
    }

    #[test]
    fn parses_language_attributes_for_the_complete_initial_language_set() {
        let attributes = [
            ("c", HighlightLanguage::C),
            ("c++", HighlightLanguage::Cpp),
            ("css", HighlightLanguage::Css),
            ("go", HighlightLanguage::Go),
            ("html", HighlightLanguage::Html),
            ("java", HighlightLanguage::Java),
            ("starlark", HighlightLanguage::Starlark),
            ("swift", HighlightLanguage::Swift),
            ("toml", HighlightLanguage::Toml),
            ("yaml", HighlightLanguage::Yaml),
        ];
        for (attribute, expected) in attributes {
            assert_eq!(
                resolve_language(Path::new("no-extension"), "", Some(attribute)),
                Some(expected),
                "{attribute}"
            );
        }
        assert_eq!(
            resolve_language(Path::new("Cargo.toml"), "", Some("not-a-language")),
            Some(HighlightLanguage::Toml),
            "an unrecognised hint must fall back to deterministic filename resolution"
        );
    }

    #[test]
    fn rust_tokens_are_side_aware_and_safe() {
        let source = include_str!("../fixtures/example.rs");
        let service = HighlightService::default();
        let result = service.highlight(&request("example.rs", source, DiffSide::New), None);
        assert_eq!(result.status, HighlightStatus::Highlighted);
        assert!(result
            .tokens
            .iter()
            .any(|token| token.class == SyntaxClass::Keyword));
        assert!(result
            .tokens
            .iter()
            .any(|token| token.class == SyntaxClass::Comment));
        assert!(result.tokens.iter().all(|token| {
            token.side == DiffSide::New
                && usize::try_from(token.end_byte).is_ok_and(|end| end <= source.len())
                && source.is_char_boundary(usize::try_from(token.start_byte).unwrap_or_default())
                && source.is_char_boundary(usize::try_from(token.end_byte).unwrap_or_default())
        }));
    }

    #[test]
    fn parses_primary_language_fixtures() {
        let fixtures = [
            ("example.js", include_str!("../fixtures/example.js")),
            ("example.ts", include_str!("../fixtures/example.ts")),
            ("example.tsx", include_str!("../fixtures/example.tsx")),
            ("example.json", include_str!("../fixtures/example.json")),
            ("example.py", include_str!("../fixtures/example.py")),
            ("example.md", include_str!("../fixtures/example.md")),
        ];
        let service = HighlightService::default();
        for (path, source) in fixtures {
            let result = service.highlight(&request(path, source, DiffSide::Old), None);
            assert_eq!(result.status, HighlightStatus::Highlighted, "{path}");
            assert!(!result.tokens.is_empty(), "{path}");
        }
    }

    #[test]
    fn parses_and_highlights_every_initial_spec_language() {
        let fixtures = [
            (
                "Sources/App.swift",
                "// greeting\nlet value: Int = 42\nprint(value)\n",
            ),
            (
                "BUILD.bazel",
                "# package rule\nload(\"//rules:defs.bzl\", \"demo\")\ndemo(name = \"app\")\n",
            ),
            (
                "Cargo.toml",
                "[package]\nname = \"localreview\"\nversion = \"0.1.0\"\n",
            ),
            (
                "config.yaml",
                "enabled: true\nname: localreview\ncount: 42\n",
            ),
            (
                "cmd/main.go",
                "package main\nimport \"fmt\"\nfunc main() { fmt.Println(42) }\n",
            ),
            (
                "src/Main.java",
                "class Main { static void main(String[] args) { System.out.println(42); } }\n",
            ),
            (
                "native/value.c",
                "#include <stdio.h>\nint main(void) { return 42; }\n",
            ),
            (
                "native/value.cpp",
                "#include <string>\nint main() { const auto value = 42; return value; }\n",
            ),
            (
                "web/page.html",
                "<!doctype html><main class=\"app\">Hello <strong>world</strong></main>\n",
            ),
            ("web/theme.css", ".app { color: #336699; margin: 1rem; }\n"),
        ];
        let service = HighlightService::default();
        for (path, source) in fixtures {
            let result = service.highlight(&request(path, source, DiffSide::New), None);
            assert_eq!(result.status, HighlightStatus::Highlighted, "{path}");
            assert!(
                !result.tokens.is_empty(),
                "{path} must produce semantic token spans"
            );
            assert!(result
                .tokens
                .iter()
                .all(|token| token.side == DiffSide::New));
        }
    }

    #[test]
    fn every_pinned_grammar_builds_a_highlight_configuration() {
        let configurations = [
            ("swift", swift_config()),
            ("starlark", starlark_config()),
            ("toml", toml_config()),
            ("yaml", yaml_config()),
            ("go", go_config()),
            ("java", java_config()),
            ("c", c_config()),
            ("cpp", cpp_config()),
            ("html", html_config()),
            ("css", css_config()),
        ];
        for (name, configuration) in configurations {
            if let Err(error) = configuration {
                panic!("{name}: {error}");
            }
        }
    }

    #[test]
    fn applies_binary_generated_and_large_file_policy() {
        let service = HighlightService::default();
        assert_eq!(
            service
                .highlight(&request("code.rs", "a\0b", DiffSide::New), None)
                .status,
            HighlightStatus::PlainText(PlainTextReason::Binary)
        );
        assert_eq!(
            service
                .highlight(
                    &request(
                        "generated.rs",
                        "// Code generated by x. DO NOT EDIT.\n",
                        DiffSide::New
                    ),
                    None
                )
                .status,
            HighlightStatus::PlainText(PlainTextReason::Generated)
        );
        let restrictive = HighlightService::new(
            HighlightPolicy {
                max_bytes: 3,
                max_lines: 1,
                highlight_generated: false,
            },
            HighlightCacheConfig::default(),
        );
        assert_eq!(
            restrictive
                .highlight(&request("x.rs", "fn x() {}", DiffSide::New), None)
                .status,
            HighlightStatus::PlainText(PlainTextReason::FileTooLarge)
        );
    }

    #[test]
    fn svelte_and_unknown_files_are_safe_fallbacks() {
        let service = HighlightService::default();
        assert_eq!(
            service
                .highlight(
                    &request(
                        "Component.svelte",
                        "<script>let x = 1;</script>",
                        DiffSide::New
                    ),
                    None
                )
                .status,
            HighlightStatus::PlainText(PlainTextReason::UnsupportedMixedLanguage)
        );
        assert_eq!(
            service
                .highlight(&request("thing.unknown", "x", DiffSide::New), None)
                .status,
            HighlightStatus::PlainText(PlainTextReason::UnknownLanguage)
        );
    }

    #[test]
    fn caches_by_side_theme_and_evicts_by_weight() {
        let service = HighlightService::new(
            HighlightPolicy::default(),
            HighlightCacheConfig {
                max_weight_bytes: 220,
            },
        );
        let one = request("one.rs", "fn one() { 1 }", DiffSide::Old);
        let two = request("two.rs", "fn two() { 2 }", DiffSide::New);
        let first = service.highlight(&one, None);
        assert!(first.key.is_some());
        let second = service.highlight(&two, None);
        assert!(second.key.is_some());
        assert!(service.cache_weight_bytes() <= 220);
        assert_ne!(first.key, second.key);
    }

    #[test]
    fn cancelled_jobs_do_not_publish_partial_tokens() {
        let service = HighlightService::default();
        let cancellation = HighlightCancellation::new();
        cancellation.cancel();
        assert_eq!(
            service
                .highlight(
                    &request("x.rs", "fn x() {}", DiffSide::New),
                    Some(&cancellation)
                )
                .status,
            HighlightStatus::PlainText(PlainTextReason::Cancelled)
        );
    }
}
