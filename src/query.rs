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

/// Convert a query string to SQL.
///
/// # Errors
/// Returns an error if the query cannot be parsed.
pub fn to_sql(query: &str) -> Result<String> {
    let mut conditions = Vec::new();
    let mut order_by = Vec::new();

    for term in query.split_whitespace() {
        if let Some(rest) = term.strip_suffix('+') {
            order_by.push(format!("{rest} ASC"));
            continue;
        }
        if let Some(rest) = term.strip_suffix('-') {
            order_by.push(format!("{rest} DESC"));
            continue;
        }

        let (negated, term) = term
            .strip_prefix('^')
            .map_or((false, term), |rest| (true, rest));

        if let Some((field, value)) = term.split_once(':') {
            let condition = parse_field_query(field, value);
            if negated {
                conditions.push(format!("NOT ({condition})"));
            } else {
                conditions.push(condition);
            }
        } else {
            conditions.push(format!(
                "id IN (SELECT rowid FROM items_fts WHERE items_fts MATCH '{}')",
                term.replace('\'', "''")
            ));
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

fn parse_field_query(field: &str, value: &str) -> String {
    // Exact match
    if let Some(exact) = value.strip_prefix('=') {
        return format!("{field} = '{}'", exact.replace('\'', "''"));
    }

    // Regex match
    if let Some(pattern) = value.strip_prefix(':') {
        let glob = regex_to_glob(pattern);
        return format!("{field} GLOB '{glob}'");
    }

    // Range match
    if value.contains("..") {
        let parts: Vec<&str> = value.split("..").collect();
        if parts.len() == 2 {
            let start = parts[0];
            let end = parts[1];
            if !start.is_empty() && !end.is_empty() {
                return format!("{field} BETWEEN '{start}' AND '{end}'");
            } else if !start.is_empty() {
                return format!("{field} >= '{start}'");
            } else if !end.is_empty() {
                return format!("{field} <= '{end}'");
            }
        }
    }

    // Relative date
    if field == "added" && value.starts_with('-') {
        if let Some(date) = parse_relative_date(value) {
            return format!("added >= '{date}'");
        }
    }

    // Substring match (default)
    format!("{field} LIKE '%{}%'", value.replace('\'', "''"))
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
}
