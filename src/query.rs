//! Typed query parser and parameterized SQL compiler.

use std::str::FromStr;

use rusqlite::types::Value;

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    terms: Vec<QueryTerm>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryTerm {
    FullText(String),
    Field {
        negated: bool,
        field: QueryField,
        operation: FieldOperation,
    },
    Sort {
        field: SortField,
        ascending: bool,
    },
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueryField {
    Path,
    Title,
    Artist,
    Album,
    AlbumArtist,
    Genre,
    Year,
    Track,
    Disc,
    Format,
    Bitrate,
    Length,
    Added,
    Modified,
    Flexible(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SortField {
    Path,
    Title,
    Artist,
    Album,
    AlbumArtist,
    Genre,
    Year,
    Track,
    Disc,
    Format,
    Bitrate,
    Length,
    Added,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FieldOperation {
    Substring(String),
    Exact(String),
    Glob(String),
    Regex(String),
    Range {
        start: Option<String>,
        end: Option<String>,
    },
    RelativeDate(String),
}

#[derive(Debug, Clone)]
pub struct CompiledQuery {
    pub sql: String,
    pub parameters: Vec<Value>,
}

impl Query {
    #[must_use]
    pub const fn all() -> Self {
        Self { terms: Vec::new() }
    }

    pub fn parse(input: &str) -> Result<Self> {
        input.parse()
    }

    pub fn compile(&self) -> CompiledQuery {
        let mut condition_groups = vec![Vec::new()];
        let mut order_by = Vec::new();
        let mut parameters = Vec::new();

        for term in &self.terms {
            match term {
                QueryTerm::FullText(text) => {
                    if let Some(conditions) = condition_groups.last_mut() {
                        conditions.push(
                            "id IN (SELECT rowid FROM items_fts WHERE items_fts MATCH ?)"
                                .to_string(),
                        );
                    }
                    parameters.push(Value::Text(format!("\"{}\"", text.replace('"', "\"\""))));
                }
                QueryTerm::Field {
                    negated,
                    field,
                    operation,
                } => {
                    let condition = compile_field(field, operation, &mut parameters);
                    if let Some(conditions) = condition_groups.last_mut() {
                        conditions.push(if *negated {
                            format!("COALESCE(NOT ({condition}), 1)")
                        } else {
                            condition
                        });
                    }
                }
                QueryTerm::Sort { field, ascending } => {
                    order_by.push(format!(
                        "{} {}",
                        field.column(),
                        if *ascending { "ASC" } else { "DESC" }
                    ));
                }
                QueryTerm::Or => condition_groups.push(Vec::new()),
            }
        }

        let conditions = condition_groups
            .into_iter()
            .filter(|group| !group.is_empty())
            .map(|group| format!("({})", group.join(" AND ")))
            .collect::<Vec<_>>();
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" OR "))
        };
        let order_clause = if order_by.is_empty() {
            " ORDER BY artist, album, disc, track".to_string()
        } else {
            format!(" ORDER BY {}", order_by.join(", "))
        };
        CompiledQuery {
            sql: format!("SELECT * FROM items{where_clause}{order_clause}"),
            parameters,
        }
    }
}

impl FromStr for Query {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self> {
        if input.trim().is_empty() {
            return Ok(Self::all());
        }

        let mut terms = Vec::new();
        for raw in tokenize(input)? {
            if raw.is_empty() {
                return Err(Error::Query("query terms cannot be empty".into()));
            }
            if raw == "," {
                if terms.is_empty() || matches!(terms.last(), Some(QueryTerm::Or)) {
                    return Err(Error::Query("OR groups cannot be empty".into()));
                }
                terms.push(QueryTerm::Or);
                continue;
            }
            if !raw.contains(':') {
                if let Some(field) = raw.strip_suffix('+') {
                    if let Ok(field) = field.parse() {
                        terms.push(QueryTerm::Sort {
                            field,
                            ascending: true,
                        });
                        continue;
                    }
                }
                if let Some(field) = raw.strip_suffix('-').filter(|field| !field.is_empty()) {
                    if let Ok(field) = field.parse() {
                        terms.push(QueryTerm::Sort {
                            field,
                            ascending: false,
                        });
                        continue;
                    }
                }
            }

            let (negated, raw) = raw
                .strip_prefix('^')
                .map_or((false, raw.as_str()), |rest| (true, rest));
            if let Some((field, value)) = raw.split_once(':') {
                if value.is_empty() {
                    return Err(Error::Query(format!("missing value for field {field}")));
                }
                let field = field.parse()?;
                terms.push(QueryTerm::Field {
                    negated,
                    operation: parse_operation(&field, value)?,
                    field,
                });
            } else {
                if negated {
                    return Err(Error::Query(
                        "negation is supported only for field queries".into(),
                    ));
                }
                terms.push(QueryTerm::FullText(raw.to_string()));
            }
        }
        if terms.iter().any(|term| matches!(term, QueryTerm::Or)) {
            let mut group_has_condition = false;
            for term in &terms {
                match term {
                    QueryTerm::FullText(_) | QueryTerm::Field { .. } => {
                        group_has_condition = true;
                    }
                    QueryTerm::Or if !group_has_condition => {
                        return Err(Error::Query("OR groups cannot be empty".into()));
                    }
                    QueryTerm::Or => group_has_condition = false,
                    QueryTerm::Sort { .. } => {}
                }
            }
            if !group_has_condition {
                return Err(Error::Query("OR groups cannot be empty".into()));
            }
        }
        Ok(Self { terms })
    }
}

fn tokenize(input: &str) -> Result<Vec<String>> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut started = false;

    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if quoted {
            match character {
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => current.push(character),
            }
            continue;
        }
        match character {
            '"' => {
                quoted = true;
                started = true;
            }
            character if character.is_whitespace() => {
                if started {
                    terms.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            _ => {
                current.push(character);
                started = true;
            }
        }
    }
    if quoted || escaped {
        return Err(Error::Query("unterminated quoted query value".into()));
    }
    if started {
        terms.push(current);
    }
    Ok(terms)
}

impl FromStr for QueryField {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "path" => Ok(Self::Path),
            "title" => Ok(Self::Title),
            "artist" => Ok(Self::Artist),
            "album" => Ok(Self::Album),
            "albumartist" | "album_artist" => Ok(Self::AlbumArtist),
            "genre" => Ok(Self::Genre),
            "year" => Ok(Self::Year),
            "track" => Ok(Self::Track),
            "disc" => Ok(Self::Disc),
            "format" => Ok(Self::Format),
            "bitrate" => Ok(Self::Bitrate),
            "length" => Ok(Self::Length),
            "added" => Ok(Self::Added),
            "mtime" | "modified" => Ok(Self::Modified),
            _ => value
                .strip_prefix("flex.")
                .filter(|field| {
                    !field.is_empty()
                        && field
                            .chars()
                            .all(|character| character.is_ascii_alphanumeric() || character == '_')
                })
                .map(|field| Self::Flexible(field.to_string()))
                .ok_or_else(|| Error::Query(format!("unknown query field: {value}"))),
        }
    }
}

impl QueryField {
    const fn column(&self) -> Option<&'static str> {
        match self {
            Self::Path => Some("path"),
            Self::Title => Some("title"),
            Self::Artist => Some("artist"),
            Self::Album => Some("album"),
            Self::AlbumArtist => Some("albumartist"),
            Self::Genre => Some("genre"),
            Self::Year => Some("year"),
            Self::Track => Some("track"),
            Self::Disc => Some("disc"),
            Self::Format => Some("format"),
            Self::Bitrate => Some("bitrate"),
            Self::Length => Some("length"),
            Self::Added => Some("added"),
            Self::Modified => Some("mtime"),
            Self::Flexible(_) => None,
        }
    }
}

impl FromStr for SortField {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let query_field: QueryField = value.parse()?;
        Ok(match query_field {
            QueryField::Path => Self::Path,
            QueryField::Title => Self::Title,
            QueryField::Artist => Self::Artist,
            QueryField::Album => Self::Album,
            QueryField::AlbumArtist => Self::AlbumArtist,
            QueryField::Genre => Self::Genre,
            QueryField::Year => Self::Year,
            QueryField::Track => Self::Track,
            QueryField::Disc => Self::Disc,
            QueryField::Format => Self::Format,
            QueryField::Bitrate => Self::Bitrate,
            QueryField::Length => Self::Length,
            QueryField::Added => Self::Added,
            QueryField::Modified => Self::Modified,
            QueryField::Flexible(_) => {
                return Err(Error::Query(
                    "flexible fields cannot be used as sort suffixes".into(),
                ))
            }
        })
    }
}

impl SortField {
    const fn column(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Title => "title",
            Self::Artist => "artist",
            Self::Album => "album",
            Self::AlbumArtist => "albumartist",
            Self::Genre => "genre",
            Self::Year => "year",
            Self::Track => "track",
            Self::Disc => "disc",
            Self::Format => "format",
            Self::Bitrate => "bitrate",
            Self::Length => "length",
            Self::Added => "added",
            Self::Modified => "mtime",
        }
    }
}

fn parse_operation(field: &QueryField, value: &str) -> Result<FieldOperation> {
    if let Some(exact) = value.strip_prefix('=') {
        return Ok(FieldOperation::Exact(exact.to_string()));
    }
    if let Some(pattern) = value.strip_prefix(':') {
        regex::Regex::new(pattern)
            .map_err(|error| Error::Query(format!("invalid regular expression: {error}")))?;
        return Ok(FieldOperation::Regex(pattern.to_string()));
    }
    if let Some(pattern) = value.strip_prefix('~') {
        return Ok(FieldOperation::Glob(pattern.to_string()));
    }
    if let Some((start, end)) = value.split_once("..") {
        return Ok(FieldOperation::Range {
            start: (!start.is_empty()).then(|| start.to_string()),
            end: (!end.is_empty()).then(|| end.to_string()),
        });
    }
    if field == &QueryField::Added && value.starts_with('-') {
        return parse_relative_date(value)
            .map(FieldOperation::RelativeDate)
            .ok_or_else(|| Error::Query(format!("invalid relative date: {value}")));
    }
    Ok(FieldOperation::Substring(value.to_string()))
}

fn compile_field(
    field: &QueryField,
    operation: &FieldOperation,
    parameters: &mut Vec<Value>,
) -> String {
    if let QueryField::Flexible(name) = field {
        parameters.push(Value::Text(name.clone()));
        let expression = "json_extract(entity_metadata.value_json, '$.value')";
        let predicate = compile_expression(expression, operation, parameters);
        return format!(
            "EXISTS (SELECT 1 FROM entity_metadata
             WHERE entity_type = 'item' AND entity_id = items.id
               AND field = ? AND {predicate})"
        );
    }
    let column = field.column().unwrap_or("id");
    compile_expression(column, operation, parameters)
}

fn compile_expression(
    column: &str,
    operation: &FieldOperation,
    parameters: &mut Vec<Value>,
) -> String {
    match operation {
        FieldOperation::Substring(value) => {
            parameters.push(Value::Text(format!("%{}%", escape_like(value))));
            format!("{column} LIKE ? ESCAPE '!'")
        }
        FieldOperation::Exact(value) => {
            parameters.push(Value::Text(value.clone()));
            format!("{column} = ?")
        }
        FieldOperation::Glob(pattern) => {
            parameters.push(Value::Text(pattern.clone()));
            format!("{column} GLOB ?")
        }
        FieldOperation::Regex(pattern) => {
            parameters.push(Value::Text(pattern.clone()));
            format!("regexp(?, {column})")
        }
        FieldOperation::Range { start, end } => match (start, end) {
            (Some(start), Some(end)) => {
                parameters.push(Value::Text(start.clone()));
                parameters.push(Value::Text(end.clone()));
                format!("{column} BETWEEN ? AND ?")
            }
            (Some(start), None) => {
                parameters.push(Value::Text(start.clone()));
                format!("{column} >= ?")
            }
            (None, Some(end)) => {
                parameters.push(Value::Text(end.clone()));
                format!("{column} <= ?")
            }
            (None, None) => format!("{column} IS NOT NULL"),
        },
        FieldOperation::RelativeDate(date) => {
            parameters.push(Value::Text(date.clone()));
            format!("{column} >= ?")
        }
    }
}

fn escape_like(value: &str) -> String {
    value
        .replace('!', "!!")
        .replace('%', "!%")
        .replace('_', "!_")
}

fn parse_relative_date(value: &str) -> Option<String> {
    let value = value.strip_prefix('-')?;
    let days = if let Some(number) = value.strip_suffix('d') {
        number.parse::<i64>().ok()?
    } else if let Some(number) = value.strip_suffix('w') {
        number.parse::<i64>().ok()?.checked_mul(7)?
    } else if let Some(number) = value.strip_suffix('m') {
        number.parse::<i64>().ok()?.checked_mul(30)?
    } else {
        let number = value.strip_suffix('y')?;
        number.parse::<i64>().ok()?.checked_mul(365)?
    };
    if days < 0 {
        return None;
    }
    let date = chrono::Utc::now().checked_sub_signed(chrono::Duration::days(days))?;
    Some(date.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn arbitrary_query_text_never_panics_or_interpolates_a_successful_parse(
            input in ".{0,2048}"
        ) {
            if let Ok(query) = Query::parse(&input) {
                let compiled = query.compile();
                prop_assert!(compiled.sql.starts_with("SELECT * FROM items"));
                prop_assert!(compiled.sql.contains(" ORDER BY "));
                prop_assert!(compiled.sql.len() <= 65_536);
            }
        }
    }

    #[test]
    fn compiles_values_as_parameters() -> Result<()> {
        let compiled = Query::parse("artist:o'brien year:1960..1969 year+")?.compile();
        assert!(compiled.sql.contains("artist LIKE ? ESCAPE '!'"));
        assert!(compiled.sql.contains("year BETWEEN ? AND ?"));
        assert!(compiled.sql.ends_with("ORDER BY year ASC"));
        assert_eq!(compiled.parameters.len(), 3);
        Ok(())
    }

    #[test]
    fn rejects_unknown_fields_and_preserves_sort_like_search_terms() -> Result<()> {
        assert!(Query::parse("invalid:value").is_err());
        assert_eq!(
            Query::parse("C++ invalid+")?.compile().parameters,
            [
                Value::Text("\"C++\"".into()),
                Value::Text("\"invalid+\"".into())
            ]
        );
        Ok(())
    }

    #[test]
    fn malformed_query_never_becomes_all() {
        assert!(Query::parse("artist:").is_err());
        assert!(Query::parse("^everything").is_err());
        assert!(Query::parse("artist:test , year+").is_err());
        assert!(Query::parse("year+ , artist:test").is_err());
    }

    #[test]
    fn full_text_is_bound() -> Result<()> {
        let compiled = Query::parse("black sabbath")?.compile();
        assert_eq!(compiled.parameters.len(), 2);
        assert!(!compiled.sql.contains("black"));
        Ok(())
    }

    #[test]
    fn quoted_values_can_contain_spaces() -> Result<()> {
        let compiled =
            Query::parse(r#"artist:"Black Sabbath" album:="Master of Reality""#)?.compile();
        assert_eq!(compiled.parameters.len(), 2);
        assert_eq!(
            compiled.parameters,
            [
                Value::Text("%Black Sabbath%".into()),
                Value::Text("Master of Reality".into())
            ]
        );
        Ok(())
    }

    #[test]
    fn field_values_may_end_in_sort_characters() -> Result<()> {
        let compiled = Query::parse("title:C++ artist:B-")?.compile();
        assert_eq!(
            compiled.parameters,
            [Value::Text("%C++%".into()), Value::Text("%B-%".into())]
        );
        Ok(())
    }

    #[test]
    fn malformed_quoted_values_fail_closed() {
        assert!(Query::parse(r#"artist:"Black Sabbath"#).is_err());
        assert!(Query::parse(r#"artist:"""#).is_err());
    }

    #[test]
    fn substring_metacharacters_are_literal_and_globs_are_not_rewritten() -> Result<()> {
        let compiled = Query::parse("title:100%_done title::Part.*")?.compile();
        assert_eq!(
            compiled.parameters,
            [
                Value::Text("%100!%!_done%".into()),
                Value::Text("Part.*".into())
            ]
        );
        Ok(())
    }

    #[test]
    fn negated_optional_fields_include_null_and_future_offsets_are_rejected() -> Result<()> {
        let compiled = Query::parse("^genre:Metal")?.compile();
        assert!(compiled
            .sql
            .contains("COALESCE(NOT (genre LIKE ? ESCAPE '!'), 1)"));
        assert!(Query::parse("added:--7d").is_err());
        Ok(())
    }
}
