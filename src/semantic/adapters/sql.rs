use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::semantic::adapter::LanguageAdapter;
use crate::semantic::common::{node_text, signature_first_line};
use crate::semantic::types::{ByteRange, ExtractedFile, Symbol, SymbolKind};

/// Tree-sitter adapter for SQL (DerekStride/tree-sitter-sql grammar,
/// published as the `tree-sitter-sequel` crate). SQL has no call
/// graph, so `find_callees_in_range` returns an empty list — the
/// value of this adapter is `list_symbols` / `get_symbol_body` /
/// `find_definition` over DDL objects.
///
/// `SymbolKind` is reused (SQL has no native fit, same as Ruby maps
/// `module`→Interface): tables/views/materialized views → `Class`
/// (a named schema object); functions → `Function`;
/// indexes → `Variable`; types → `TypeAlias`. The `signature` field
/// carries the full `CREATE …` line so the kind is only a coarse
/// filter.
pub struct SqlAdapter;

impl SqlAdapter {
    /// The object name is the first `identifier` leaf in document
    /// order: DDL always names the object before any column /
    /// parameter / body list, so the first identifier encountered is
    /// the object name (`CREATE TABLE users (...)` → `users`,
    /// `CREATE INDEX idx ON users(col)` → `idx`). Keywords are
    /// `keyword_*` nodes, never `identifier`, so `IF NOT EXISTS` and
    /// `TEMPORARY` don't interfere.
    fn first_identifier(&self, n: Node, s: &[u8]) -> Option<String> {
        if n.kind() == "identifier" {
            return Some(node_text(n, s).to_string());
        }
        let mut cursor = n.walk();
        for child in n.named_children(&mut cursor) {
            if let Some(name) = self.first_identifier(child, s) {
                return Some(name);
            }
        }
        None
    }

    fn emit(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>, kind: SymbolKind) {
        let Some(name) = self.first_identifier(n, s) else {
            return;
        };
        symbols.push(Symbol {
            kind,
            is_exported: true,
            name,
            range: ByteRange::from(n),
            signature: signature_first_line(n, s),
            parent_class: None,
        });
    }

    /// Recursively scan for DDL nodes. The grammar wraps each
    /// top-level statement in `statement` (and `create_statement`),
    /// which we recurse through transparently. `create_*` nodes never
    /// nest, so each fires exactly once.
    fn walk(&self, n: Node, s: &[u8], symbols: &mut Vec<Symbol>) {
        match n.kind() {
            "create_table" | "create_view" | "create_materialized_view" => {
                self.emit(n, s, symbols, SymbolKind::Class);
            }
            "create_function" => {
                self.emit(n, s, symbols, SymbolKind::Function);
            }
            "create_index" => {
                self.emit(n, s, symbols, SymbolKind::Variable);
            }
            "create_type" => {
                self.emit(n, s, symbols, SymbolKind::TypeAlias);
            }
            _ => {}
        }
        let mut cursor = n.walk();
        for child in n.named_children(&mut cursor) {
            self.walk(child, s, symbols);
        }
    }
}

impl LanguageAdapter for SqlAdapter {
    fn extensions(&self) -> &[&str] {
        &[".sql"]
    }

    fn extract(&self, file_path: &Path, source: &str) -> Result<ExtractedFile, String> {
        let lang: tree_sitter::Language = tree_sitter_sequel::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&lang)
            .map_err(|e| format!("Failed to set language: {e}"))?;
        let tree = parser.parse(source, None).ok_or("Failed to parse source")?;
        let root = tree.root_node();
        let source_bytes = source.as_bytes();

        let mut symbols = Vec::new();
        let mut warnings = Vec::new();

        if root.has_error() {
            warnings.push("tree-sitter reported syntax errors".to_string());
        }

        self.walk(root, source_bytes, &mut symbols);

        let exports: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();

        Ok(ExtractedFile {
            file_path: file_path.to_path_buf(),
            symbols,
            imports: vec![],
            exports,
            warnings,
            mtime: std::time::SystemTime::now(),
            size: 0,
            head_hash: 0,
        })
    }

    fn find_callees_in_range(
        &self,
        _source: &str,
        _file_path: &Path,
        _range: ByteRange,
    ) -> Result<Vec<String>, String> {
        // SQL has no call graph — DDL objects aren't invoked. Return
        // empty so find_callees / find_callers no-op for SQL.
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(name)
    }

    #[test]
    fn extracts_create_table_and_view() {
        let src = "CREATE TABLE users (\n  id INT PRIMARY KEY,\n  email TEXT NOT NULL\n);\n\
                   CREATE VIEW active_users AS SELECT * FROM users WHERE active;\n";
        let f = SqlAdapter.extract(&pb("schema.sql"), src).unwrap();
        let t = f.symbols.iter().find(|s| s.name == "users").unwrap();
        assert!(matches!(t.kind, SymbolKind::Class));
        let v = f.symbols.iter().find(|s| s.name == "active_users").unwrap();
        assert!(matches!(v.kind, SymbolKind::Class));
    }

    #[test]
    fn extracts_function() {
        let src =
            "CREATE FUNCTION add(a INT, b INT) RETURNS INT AS $$ SELECT a + b $$ LANGUAGE SQL;\n";
        let f = SqlAdapter.extract(&pb("fns.sql"), src).unwrap();
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "add" && matches!(s.kind, SymbolKind::Function))
        );
    }

    #[test]
    fn extracts_index_and_type() {
        let src = "CREATE INDEX idx_email ON users(email);\n\
                   CREATE TYPE mood AS ENUM ('happy','sad');\n";
        let f = SqlAdapter.extract(&pb("misc.sql"), src).unwrap();
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "idx_email" && matches!(s.kind, SymbolKind::Variable))
        );
        assert!(
            f.symbols
                .iter()
                .any(|s| s.name == "mood" && matches!(s.kind, SymbolKind::TypeAlias))
        );
    }

    #[test]
    fn find_callees_is_empty() {
        let src = "CREATE FUNCTION f() RETURNS INT AS $$ SELECT 1 $$ LANGUAGE SQL;\n";
        let f = SqlAdapter.extract(&pb("f.sql"), src).unwrap();
        let sym = f.symbols.first().unwrap();
        let callees = SqlAdapter
            .find_callees_in_range(src, &pb("f.sql"), sym.range)
            .unwrap();
        assert!(callees.is_empty());
    }

    #[test]
    fn extracts_materialized_view() {
        let src = "CREATE MATERIALIZED VIEW mv_stats AS SELECT count(*) FROM events;\n";
        let f = SqlAdapter.extract(&pb("mv.sql"), src).unwrap();
        assert!(f.warnings.is_empty());
        let mv = f.symbols.iter().find(|s| s.name == "mv_stats").unwrap();
        assert!(matches!(mv.kind, SymbolKind::Class));
    }

    #[test]
    fn broken_sql_emits_warning() {
        let src = "CREATE TABLE users (id INT;\n";
        let f = SqlAdapter.extract(&pb("bad.sql"), src).unwrap();
        assert!(!f.warnings.is_empty());
    }

    #[test]
    fn pure_select_extracts_nothing() {
        let src = "SELECT id, name FROM users WHERE active = TRUE;\n";
        let f = SqlAdapter.extract(&pb("q.sql"), src).unwrap();
        assert!(f.symbols.is_empty());
        assert!(f.warnings.is_empty());
    }

    #[test]
    fn qualified_name_yields_schema_not_table() {
        // Known v1 limitation: the first identifier leaf in document
        // order is the schema qualifier, not the table name. Locks in
        // current behavior so a grammar fix surfaces as a failure.
        let src = "CREATE TABLE public.users (id INT);\n";
        let f = SqlAdapter.extract(&pb("schema.sql"), src).unwrap();
        assert!(f.symbols.iter().any(|s| s.name == "public"));
        assert!(!f.symbols.iter().any(|s| s.name == "users"));
    }
}
