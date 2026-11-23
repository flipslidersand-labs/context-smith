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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_from_path_maps_known_extensions() {
        let cases = [
            ("a.rs", Language::Rust),
            ("a.py", Language::Python),
            ("a.go", Language::Go),
            ("a.ts", Language::TypeScript),
            ("a.tsx", Language::TypeScript),
            ("a.mts", Language::TypeScript),
            ("a.cts", Language::TypeScript),
            ("a.js", Language::JavaScript),
            ("a.jsx", Language::JavaScript),
            ("a.mjs", Language::JavaScript),
            ("a.cjs", Language::JavaScript),
            ("a.rb", Language::Ruby),
        ];
        for (p, expected) in cases {
            assert_eq!(Language::from_path(Path::new(p)), expected, "{p}");
        }
    }

    #[test]
    fn language_from_path_unknown_and_no_extension() {
        assert_eq!(Language::from_path(Path::new("README")), Language::Unknown);
        assert_eq!(Language::from_path(Path::new("a.txt")), Language::Unknown);
        assert_eq!(Language::from_path(Path::new("")), Language::Unknown);
        // Extension matching is case-sensitive.
        assert_eq!(Language::from_path(Path::new("A.RS")), Language::Unknown);
    }

    #[test]
    fn language_as_str_values() {
        assert_eq!(Language::Rust.as_str(), "rust");
        assert_eq!(Language::Python.as_str(), "python");
        assert_eq!(Language::Go.as_str(), "go");
        assert_eq!(Language::TypeScript.as_str(), "typescript");
        assert_eq!(Language::JavaScript.as_str(), "javascript");
        assert_eq!(Language::Ruby.as_str(), "ruby");
        assert_eq!(Language::Unknown.as_str(), "unknown");
    }

    #[test]
    fn symbol_kind_as_str_values() {
        assert_eq!(SymbolKind::Function.as_str(), "function");
        assert_eq!(SymbolKind::Struct.as_str(), "struct");
        assert_eq!(SymbolKind::Class.as_str(), "class");
        assert_eq!(SymbolKind::Impl.as_str(), "impl");
        assert_eq!(SymbolKind::Import.as_str(), "import");
    }
}
