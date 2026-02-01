//! Query language parser
//!
//! Syntax:
//!   keyword                   - FTS search
//!   `artist:beatles`          - Field substring
//!   `title:=Help!`            - Exact match
//!   `genre::^rock`            - Regex/glob
//!   `year:1960..1969`         - Range
//!   `^genre:jazz`             - Negation

use crate::Result;

/// A parsed query term in the AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryTerm {
    /// Full-text search term
    FullText(String),
    /// Field-based filter
    Field {
        negated: bool,
        name: String,
        op: FieldOp,
    },
    /// Sort directive
    Sort { field: String, ascending: bool },
}

/// Field operation types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldOp {
    /// Substring match (default): field LIKE '%value%'
    Substring(String),
    /// Exact match: field = 'value'
    Exact(String),
    /// Regex/glob match: field GLOB 'pattern'
    Regex(String),
    /// Range match: field BETWEEN start AND end
    Range {
        start: Option<String>,
        end: Option<String>,
    },
    /// Relative date: added >= 'date'
    RelativeDate(String),
}

/// Parse a query string into AST terms.
///
/// # Errors
/// Returns an error if parsing fails.
pub fn parse(query: &str) -> Result<Vec<QueryTerm>> {
    let mut terms = Vec::new();

    for term in query.split_whitespace() {
        // Sort directive (ascending)
        if let Some(rest) = term.strip_suffix('+') {
            terms.push(QueryTerm::Sort {
                field: rest.to_string(),
                ascending: true,
            });
            continue;
        }

        // Sort directive (descending)
        if let Some(rest) = term.strip_suffix('-') {
            terms.push(QueryTerm::Sort {
                field: rest.to_string(),
                ascending: false,
            });
            continue;
        }

        // Negation prefix
        let (negated, term) = term
            .strip_prefix('^')
            .map_or((false, term), |rest| (true, rest));

        if let Some((field, value)) = term.split_once(':') {
            let op = parse_field_op(field, value);
            terms.push(QueryTerm::Field {
                negated,
                name: field.to_string(),
                op,
            });
        } else {
            terms.push(QueryTerm::FullText(term.to_string()));
        }
    }

    Ok(terms)
}

/// Parse a field operation from the value string.
fn parse_field_op(field: &str, value: &str) -> FieldOp {
    // Exact match
    if let Some(exact) = value.strip_prefix('=') {
        return FieldOp::Exact(exact.to_string());
    }

    // Regex/glob match
    if let Some(pattern) = value.strip_prefix(':') {
        return FieldOp::Regex(pattern.to_string());
    }

    // Range match
    if value.contains("..") {
        let parts: Vec<&str> = value.split("..").collect();
        if parts.len() == 2 {
            let start = if parts[0].is_empty() {
                None
            } else {
                Some(parts[0].to_string())
            };
            let end = if parts[1].is_empty() {
                None
            } else {
                Some(parts[1].to_string())
            };
            return FieldOp::Range { start, end };
        }
    }

    // Relative date
    if field == "added" && value.starts_with('-') {
        if let Some(date) = parse_relative_date(value) {
            return FieldOp::RelativeDate(date);
        }
    }

    // Substring match (default)
    FieldOp::Substring(value.to_string())
}

/// Convert AST terms to SQL.
///
/// # Errors
/// Returns an error if SQL generation fails.
pub fn terms_to_sql(terms: &[QueryTerm]) -> Result<String> {
    let mut conditions = Vec::new();
    let mut order_by = Vec::new();

    for term in terms {
        match term {
            QueryTerm::FullText(text) => {
                conditions.push(format!(
                    "id IN (SELECT rowid FROM items_fts WHERE items_fts MATCH '{}')",
                    text.replace('\'', "''")
                ));
            }
            QueryTerm::Field { negated, name, op } => {
                let condition = field_op_to_sql(name, op);
                if *negated {
                    conditions.push(format!("NOT ({condition})"));
                } else {
                    conditions.push(condition);
                }
            }
            QueryTerm::Sort { field, ascending } => {
                let direction = if *ascending { "ASC" } else { "DESC" };
                order_by.push(format!("{field} {direction}"));
            }
        }
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let order_clause = if order_by.is_empty() {
        "ORDER BY artist, album, disc, track".to_string()
    } else {
        format!("ORDER BY {}", order_by.join(", "))
    };

    Ok(format!("SELECT * FROM items {where_clause} {order_clause}"))
}

/// Convert a field operation to SQL.
fn field_op_to_sql(field: &str, op: &FieldOp) -> String {
    match op {
        FieldOp::Substring(value) => {
            format!("{field} LIKE '%{}%'", value.replace('\'', "''"))
        }
        FieldOp::Exact(value) => {
            format!("{field} = '{}'", value.replace('\'', "''"))
        }
        FieldOp::Regex(pattern) => {
            let glob = regex_to_glob(pattern);
            format!("{field} GLOB '{glob}'")
        }
        FieldOp::Range { start, end } => match (start, end) {
            (Some(s), Some(e)) => format!("{field} BETWEEN '{s}' AND '{e}'"),
            (Some(s), None) => format!("{field} >= '{s}'"),
            (None, Some(e)) => format!("{field} <= '{e}'"),
            (None, None) => format!("{field} IS NOT NULL"),
        },
        FieldOp::RelativeDate(date) => {
            format!("{field} >= '{date}'")
        }
    }
}

/// Convert a query string to SQL.
///
/// # Errors
/// Returns an error if the query cannot be parsed.
pub fn to_sql(query: &str) -> Result<String> {
    let terms = parse(query)?;
    terms_to_sql(&terms)
}

fn regex_to_glob(pattern: &str) -> String {
    pattern
        .replace(".*", "*")
        .replace('.', "?")
        .replace(['^', '$'], "")
}

fn parse_relative_date(value: &str) -> Option<String> {
    let value = value.trim_start_matches('-');
    let num = if value.ends_with('d') {
        value.trim_end_matches('d').parse::<i64>().ok()?
    } else if value.ends_with('w') {
        value.trim_end_matches('w').parse::<i64>().ok()? * 7
    } else if value.ends_with('m') {
        value.trim_end_matches('m').parse::<i64>().ok()? * 30
    } else if value.ends_with('y') {
        value.trim_end_matches('y').parse::<i64>().ok()? * 365
    } else {
        return None;
    };

    let date = chrono::Utc::now() - chrono::Duration::days(num);
    Some(date.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_query() {
        let sql = to_sql("artist:beatles").unwrap();
        assert!(sql.contains("artist LIKE '%beatles%'"));
    }

    #[test]
    fn test_exact_match() {
        let sql = to_sql("title:=Help!").unwrap();
        assert!(sql.contains("title = 'Help!'"));
    }

    #[test]
    fn test_range() {
        let sql = to_sql("year:1960..1969").unwrap();
        assert!(sql.contains("year BETWEEN '1960' AND '1969'"));
    }

    #[test]
    fn test_negation() {
        let sql = to_sql("^genre:jazz").unwrap();
        assert!(sql.contains("NOT (genre LIKE '%jazz%')"));
    }

    #[test]
    fn test_parse_fulltext() {
        let terms = parse("beatles").unwrap();
        assert_eq!(terms.len(), 1);
        assert!(matches!(&terms[0], QueryTerm::FullText(s) if s == "beatles"));
    }

    #[test]
    fn test_parse_field() {
        let terms = parse("artist:beatles").unwrap();
        assert_eq!(terms.len(), 1);
        assert!(matches!(
            &terms[0],
            QueryTerm::Field {
                negated: false,
                name,
                op: FieldOp::Substring(v)
            } if name == "artist" && v == "beatles"
        ));
    }

    #[test]
    fn test_parse_sort() {
        let terms = parse("year+").unwrap();
        assert_eq!(terms.len(), 1);
        assert!(matches!(
            &terms[0],
            QueryTerm::Sort {
                field,
                ascending: true
            } if field == "year"
        ));
    }
}
