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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        let mut conditions = Vec::new();
        let mut order_by = Vec::new();
        let mut parameters = Vec::new();

        for term in &self.terms {
            match term {
                QueryTerm::FullText(text) => {
                    conditions.push(
                        "id IN (SELECT rowid FROM items_fts WHERE items_fts MATCH ?)".to_string(),
                    );
                    parameters.push(Value::Text(format!("\"{}\"", text.replace('"', "\"\""))));
                }
                QueryTerm::Field {
                    negated,
                    field,
                    operation,
                } => {
                    let condition = compile_field(*field, operation, &mut parameters);
                    conditions.push(if *negated {
                        format!("NOT ({condition})")
                    } else {
                        condition
                    });
                }
                QueryTerm::Sort { field, ascending } => {
                    order_by.push(format!(
                        "{} {}",
                        field.column(),
                        if *ascending { "ASC" } else { "DESC" }
                    ));
                }
            }
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
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
        for raw in input.split_whitespace() {
            if let Some(field) = raw.strip_suffix('+') {
                terms.push(QueryTerm::Sort {
                    field: field.parse()?,
                    ascending: true,
                });
                continue;
            }
            if let Some(field) = raw.strip_suffix('-').filter(|field| !field.is_empty()) {
                terms.push(QueryTerm::Sort {
                    field: field.parse()?,
                    ascending: false,
                });
                continue;
            }

            let (negated, raw) = raw
                .strip_prefix('^')
                .map_or((false, raw), |rest| (true, rest));
            if let Some((field, value)) = raw.split_once(':') {
                if value.is_empty() {
                    return Err(Error::Query(format!("missing value for field {field}")));
                }
                let field = field.parse()?;
                terms.push(QueryTerm::Field {
                    negated,
                    field,
                    operation: parse_operation(field, value)?,
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
        Ok(Self { terms })
    }
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
            _ => Err(Error::Query(format!("unknown query field: {value}"))),
        }
    }
}

impl QueryField {
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

fn parse_operation(field: QueryField, value: &str) -> Result<FieldOperation> {
    if let Some(exact) = value.strip_prefix('=') {
        return Ok(FieldOperation::Exact(exact.to_string()));
    }
    if let Some(pattern) = value.strip_prefix(':') {
        return Ok(FieldOperation::Glob(regex_to_glob(pattern)));
    }
    if let Some((start, end)) = value.split_once("..") {
        return Ok(FieldOperation::Range {
            start: (!start.is_empty()).then(|| start.to_string()),
            end: (!end.is_empty()).then(|| end.to_string()),
        });
    }
    if field == QueryField::Added && value.starts_with('-') {
        return parse_relative_date(value)
            .map(FieldOperation::RelativeDate)
            .ok_or_else(|| Error::Query(format!("invalid relative date: {value}")));
    }
    Ok(FieldOperation::Substring(value.to_string()))
}

fn compile_field(
    field: QueryField,
    operation: &FieldOperation,
    parameters: &mut Vec<Value>,
) -> String {
    let column = field.column();
    match operation {
        FieldOperation::Substring(value) => {
            parameters.push(Value::Text(format!("%{value}%")));
            format!("{column} LIKE ?")
        }
        FieldOperation::Exact(value) => {
            parameters.push(Value::Text(value.clone()));
            format!("{column} = ?")
        }
        FieldOperation::Glob(pattern) => {
            parameters.push(Value::Text(pattern.clone()));
            format!("{column} GLOB ?")
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

fn regex_to_glob(pattern: &str) -> String {
    pattern
        .replace(".*", "*")
        .replace('.', "?")
        .replace(['^', '$'], "")
}

fn parse_relative_date(value: &str) -> Option<String> {
    let value = value.strip_prefix('-')?;
    let days = if let Some(number) = value.strip_suffix('d') {
        number.parse::<i64>().ok()?
    } else if let Some(number) = value.strip_suffix('w') {
        number.parse::<i64>().ok()?.checked_mul(7)?
    } else if let Some(number) = value.strip_suffix('m') {
        number.parse::<i64>().ok()?.checked_mul(30)?
    } else if let Some(number) = value.strip_suffix('y') {
        number.parse::<i64>().ok()?.checked_mul(365)?
    } else {
        return None;
    };
    let date = chrono::Utc::now().checked_sub_signed(chrono::Duration::days(days))?;
    Some(date.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_values_as_parameters() -> Result<()> {
        let compiled = Query::parse("artist:o'brien year:1960..1969 year+")?.compile();
        assert!(compiled.sql.contains("artist LIKE ?"));
        assert!(compiled.sql.contains("year BETWEEN ? AND ?"));
        assert!(compiled.sql.ends_with("ORDER BY year ASC"));
        assert_eq!(compiled.parameters.len(), 3);
        Ok(())
    }

    #[test]
    fn rejects_unknown_fields_and_sort_keys() {
        assert!(Query::parse("invalid:value").is_err());
        assert!(Query::parse("invalid+").is_err());
    }

    #[test]
    fn malformed_query_never_becomes_all() {
        assert!(Query::parse("artist:").is_err());
        assert!(Query::parse("^everything").is_err());
    }

    #[test]
    fn full_text_is_bound() -> Result<()> {
        let compiled = Query::parse("black sabbath")?.compile();
        assert_eq!(compiled.parameters.len(), 2);
        assert!(!compiled.sql.contains("black"));
        Ok(())
    }
}
