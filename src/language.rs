use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    Rust,
    Python,
    Go,
    TypeScript,
    JavaScript,
    Ruby,
    Unknown,
}

impl Language {
    pub(crate) fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Language::Rust,
            Some("py") => Language::Python,
            Some("go") => Language::Go,
            Some("ts" | "tsx" | "mts" | "cts") => Language::TypeScript,
            Some("js" | "jsx" | "mjs" | "cjs") => Language::JavaScript,
            Some("rb") => Language::Ruby,
            _ => Language::Unknown,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::Go => "go",
            Language::TypeScript => "typescript",
            Language::JavaScript => "javascript",
            Language::Ruby => "ruby",
            Language::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SymbolKind {
    Function,
    Struct,
    Class,
    Impl,
    Import,
}

impl SymbolKind {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Struct => "struct",
            SymbolKind::Class => "class",
            SymbolKind::Impl => "impl",
            SymbolKind::Import => "import",
        }
    }
}
