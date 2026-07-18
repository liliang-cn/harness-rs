//! Read-only SQL guard.
//!
//! The primary control against an agent mutating a production ERP/MES database
//! is to connect with a **read-only database account**. This guard is
//! defense-in-depth on top of that: it rejects anything that isn't a single
//! `SELECT`/`WITH` statement, and appends a `LIMIT` so a runaway query can't
//! drag rows without bound. It errs strict — a query rejected is an annoyance; a
//! write slipping through is not acceptable on a customer's live system.

use regex::Regex;
use std::sync::OnceLock;

/// Why a statement was refused by [`check_read_only`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GuardError {
    #[error("empty statement")]
    Empty,
    #[error("only a single read-only SELECT/WITH statement is allowed")]
    NotReadOnly,
    #[error("multiple statements are not allowed")]
    MultipleStatements,
    #[error("write/DDL keyword `{0}` is not allowed in a read-only query")]
    WriteKeyword(String),
}

/// Word-boundary match on any statement that writes or changes schema. `\b`
/// means an identifier like `update_time` or `created_at` does *not* trip it.
fn write_keyword_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?i)\b(INSERT|UPDATE|DELETE|DROP|ALTER|CREATE|TRUNCATE|MERGE|REPLACE|UPSERT|GRANT|REVOKE|EXEC|EXECUTE|CALL|ATTACH|DETACH|PRAGMA|VACUUM|INTO)\b",
        )
        .unwrap()
    })
}

fn limit_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\bLIMIT\b").unwrap())
}

/// Validate that `sql` is a single read-only `SELECT` (or `WITH … SELECT`).
pub fn check_read_only(sql: &str) -> Result<(), GuardError> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err(GuardError::Empty);
    }
    // Any remaining `;` means a second statement was smuggled in.
    if trimmed.contains(';') {
        return Err(GuardError::MultipleStatements);
    }
    let first = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if first != "SELECT" && first != "WITH" {
        return Err(GuardError::NotReadOnly);
    }
    if let Some(m) = write_keyword_re().find(trimmed) {
        return Err(GuardError::WriteKeyword(m.as_str().to_ascii_uppercase()));
    }
    Ok(())
}

/// Append `LIMIT max` when the query has no `LIMIT` of its own, so results are
/// always bounded. Assumes a `LIMIT`-dialect (SQLite / MySQL / PostgreSQL);
/// a `TOP`-dialect executor (SQL Server) should bound rows itself.
pub fn enforce_limit(sql: &str, max: u32) -> String {
    let base = sql.trim().trim_end_matches(';').trim();
    if limit_re().is_match(base) {
        base.to_string()
    } else {
        format!("{base} LIMIT {max}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_select_and_cte() {
        assert!(check_read_only("SELECT * FROM work_orders WHERE qty > 0").is_ok());
        assert!(check_read_only("  select id from t ;").is_ok());
        assert!(check_read_only("WITH x AS (SELECT 1) SELECT * FROM x").is_ok());
    }

    #[test]
    fn rejects_writes_and_ddl() {
        assert_eq!(
            check_read_only("DELETE FROM work_orders"),
            Err(GuardError::NotReadOnly)
        );
        // Write keyword hidden after a legit SELECT prefix.
        assert!(matches!(
            check_read_only("SELECT 1; DROP TABLE t"),
            Err(GuardError::MultipleStatements)
        ));
        assert!(matches!(
            check_read_only("SELECT * INTO backup FROM t"),
            Err(GuardError::WriteKeyword(_))
        ));
    }

    #[test]
    fn identifiers_containing_keywords_are_fine() {
        // `update_time` / `created_at` must not trip the write-keyword guard.
        assert!(check_read_only("SELECT update_time, created_at FROM orders").is_ok());
    }

    #[test]
    fn limit_is_appended_only_when_absent() {
        assert_eq!(
            enforce_limit("SELECT * FROM t", 100),
            "SELECT * FROM t LIMIT 100"
        );
        assert_eq!(
            enforce_limit("SELECT * FROM t LIMIT 5", 100),
            "SELECT * FROM t LIMIT 5"
        );
    }
}
