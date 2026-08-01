//! Path template engine
//!
//! Syntax:
//!   `$field` - Variable substitution
//!   `%func{arg}` - Function call
//!
//! Variables: albumartist, artist, album, year, track, title, disc, genre
//! Functions: upper, lower, if, left, right

use std::path::{Component, Path, PathBuf};

use crate::{Error, Item, Result};

/// Format a path template with item metadata.
///
/// # Errors
/// Returns an error if the template contains unknown variables or functions.
pub fn format_path(template: &str, item: &Item) -> Result<String> {
    validate_template(template)?;
    render_path(template, item)
}

fn render_path(template: &str, item: &Item) -> Result<String> {
    let mut result = String::new();
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '$' => {
                let var = collect_identifier(&mut chars);
                let value = get_variable(&var, item)?;
                result.push_str(&sanitize_substitution(&value));
            }
            '%' => {
                let func = collect_identifier(&mut chars);
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let arg = collect_until_close(&mut chars)?;
                    let value = apply_function(&func, &arg, item)?;
                    result.push_str(&sanitize_substitution(&value));
                } else {
                    return Err(Error::PathFormat(format!("Expected '{{' after %{func}")));
                }
            }
            _ => result.push(c),
        }
    }

    Ok(result)
}

/// Validate template syntax and identifiers without requiring track metadata.
///
/// # Errors
/// Returns an error for unknown fields or functions, malformed function arguments, and
/// unbalanced delimiters.
pub fn validate_template(template: &str) -> Result<()> {
    let mut chars = template.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '$' => {
                let variable = collect_identifier(&mut chars);
                validate_variable(&variable)?;
            }
            '%' => {
                let function = collect_identifier(&mut chars);
                if chars.next() != Some('{') {
                    return Err(Error::PathFormat(format!(
                        "expected '{{' after %{function}"
                    )));
                }
                let argument = collect_until_close(&mut chars)?;
                validate_function(&function, &argument)?;
            }
            '}' => return Err(Error::PathFormat("unexpected closing '}'".into())),
            _ => {}
        }
    }
    Ok(())
}

/// Format and validate a relative destination path.
pub fn format_relative_path(template: &str, item: &Item) -> Result<PathBuf> {
    let formatted = format_path(template, item)?;
    if formatted.chars().any(char::is_control) {
        return Err(Error::PathFormat(
            "path template produced a control character".into(),
        ));
    }
    let path = Path::new(&formatted);
    if path.is_absolute() {
        return Err(Error::PathFormat(
            "path template produced an absolute path".into(),
        ));
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) if value != "." && value != ".." => safe.push(value),
            Component::CurDir => {}
            Component::Normal(_)
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(Error::PathFormat(format!(
                    "unsafe path component in formatted path: {formatted}"
                )));
            }
        }
    }
    if safe.as_os_str().is_empty() {
        return Err(Error::PathFormat(
            "path template produced an empty path".into(),
        ));
    }
    Ok(safe)
}

fn collect_identifier(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut ident = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || c == '_' {
            ident.push(c);
            chars.next();
        } else {
            break;
        }
    }
    ident
}

fn collect_until_close(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<String> {
    let mut content = String::new();
    let mut depth = 1;
    for c in chars.by_ref() {
        match c {
            '{' => {
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(content);
                }
            }
            _ => {}
        }
        content.push(c);
    }
    Err(Error::PathFormat("missing closing '}'".into()))
}

fn get_variable(name: &str, item: &Item) -> Result<String> {
    Ok(match name {
        "title" => item.title.clone(),
        "artist" => item.artist.clone(),
        "album" => item.album.clone(),
        "albumartist" => item.effective_albumartist().to_string(),
        "genre" => item.genre.clone().unwrap_or_default(),
        "year" => item.year.map_or_else(String::new, |y| y.to_string()),
        "track" => item.track.map_or_else(String::new, |t| format!("{t:02}")),
        "disc" => item.disc.map_or_else(String::new, |d| d.to_string()),
        _ => return Err(Error::PathFormat(format!("Unknown variable: {name}"))),
    })
}

fn validate_variable(name: &str) -> Result<()> {
    if matches!(
        name,
        "title" | "artist" | "album" | "albumartist" | "genre" | "year" | "track" | "disc"
    ) {
        Ok(())
    } else {
        Err(Error::PathFormat(format!("unknown variable: {name}")))
    }
}

fn validate_function(function: &str, argument: &str) -> Result<()> {
    match function {
        "upper" | "lower" | "title" => validate_template(argument),
        "left" | "right" => {
            let parts = split_arguments(argument)?;
            if parts.len() != 2 {
                return Err(Error::PathFormat(format!(
                    "%{function} expects a length and a value"
                )));
            }
            parts[0].trim().parse::<usize>().map_err(|error| {
                Error::PathFormat(format!("invalid %{function} length: {error}"))
            })?;
            validate_template(parts[1].trim())
        }
        "if" => {
            let parts = split_arguments(argument)?;
            if !(2..=3).contains(&parts.len()) {
                return Err(Error::PathFormat(
                    "%if expects a condition, true value, and optional false value".into(),
                ));
            }
            for part in parts {
                validate_template(part.trim())?;
            }
            Ok(())
        }
        _ => Err(Error::PathFormat(format!("unknown function: {function}"))),
    }
}

fn split_arguments(argument: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0_u32;
    for (index, character) in argument.char_indices() {
        match character {
            '{' => depth = depth.saturating_add(1),
            '}' => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    Error::PathFormat("unexpected closing '}' in function arguments".into())
                })?;
            }
            ',' if depth == 0 => {
                parts.push(&argument[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(Error::PathFormat(
            "unclosed function in function arguments".into(),
        ));
    }
    parts.push(&argument[start..]);
    Ok(parts)
}

fn apply_function(func: &str, arg: &str, item: &Item) -> Result<String> {
    validate_function(func, arg)?;
    Ok(match func {
        "upper" => render_path(arg, item)?.to_uppercase(),
        "lower" => render_path(arg, item)?.to_lowercase(),
        "title" => to_title_case(&render_path(arg, item)?),
        "left" => {
            let parts = split_arguments(arg)?;
            let length = parts[0]
                .trim()
                .parse::<usize>()
                .map_err(|error| Error::PathFormat(format!("invalid %left length: {error}")))?;
            render_path(parts[1].trim(), item)?
                .chars()
                .take(length)
                .collect()
        }
        "right" => {
            let parts = split_arguments(arg)?;
            let length = parts[0]
                .trim()
                .parse::<usize>()
                .map_err(|error| Error::PathFormat(format!("invalid %right length: {error}")))?;
            let value = render_path(parts[1].trim(), item)?;
            let character_count = value.chars().count();
            value
                .chars()
                .skip(character_count.saturating_sub(length))
                .collect()
        }
        "if" => {
            let parts = split_arguments(arg)?;
            let condition = render_path(parts[0].trim(), item)?;
            if !condition.is_empty() {
                render_path(parts[1].trim(), item)?
            } else if parts.len() == 3 {
                render_path(parts[2].trim(), item)?
            } else {
                String::new()
            }
        }
        _ => return Err(Error::PathFormat(format!("Unknown function: {func}"))),
    })
}

fn to_title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase()
                    .chain(chars.flat_map(char::to_lowercase))
                    .collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_substitution(s: &str) -> String {
    let sanitized = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized == "." || sanitized == ".." {
        "_".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn test_item() -> Item {
        Item {
            id: None,
            album_id: None,
            path: "/test.mp3".into(),
            title: "Help!".into(),
            artist: "The Beatles".into(),
            album: "Help!".into(),
            albumartist: None,
            genre: Some("Rock".into()),
            year: Some(1965),
            track: Some(1),
            disc: Some(1),
            format: crate::AudioFormat::Mp3,
            bitrate: 320,
            length: 180.0,
            file_size: Some(1),
            track_external_id: None,
            release_external_id: None,
            added: Utc::now(),
            mtime: Utc::now(),
        }
    }

    #[test]
    fn test_simple_template() -> Result<()> {
        let item = test_item();
        let result = format_path("$artist/$album/$track - $title", &item)?;
        assert_eq!(result, "The Beatles/Help!/01 - Help!");
        Ok(())
    }

    #[test]
    fn test_functions() -> Result<()> {
        let item = test_item();
        let result = format_path("%upper{$artist}", &item)?;
        assert_eq!(result, "THE BEATLES");
        Ok(())
    }

    #[test]
    fn rejects_absolute_and_parent_paths() {
        let item = test_item();
        assert!(format_relative_path("/$artist/$title", &item).is_err());
        assert!(format_relative_path("../$artist/$title", &item).is_err());
    }

    #[test]
    fn neutralizes_dot_metadata_components() -> Result<()> {
        let mut item = test_item();
        item.artist = "..".into();
        assert_eq!(
            format_relative_path("$artist/$title", &item)?,
            PathBuf::from("_/Help!")
        );
        Ok(())
    }

    #[test]
    fn neutralizes_control_characters() -> Result<()> {
        let mut item = test_item();
        item.title = "line one\nline two".into();
        assert_eq!(format_path("$title", &item)?, "line one_line two");
        Ok(())
    }

    #[test]
    fn rejects_literal_control_characters() {
        assert!(format_relative_path("album\n/$title", &test_item()).is_err());
    }

    #[test]
    fn rejects_unclosed_functions() {
        assert!(format_path("%upper{$artist", &test_item()).is_err());
    }

    #[test]
    fn optional_values_drive_if_branches() -> Result<()> {
        let mut item = test_item();
        item.genre = None;
        assert_eq!(format_path("%if{$genre,$genre,Unknown}", &item)?, "Unknown");
        item.genre = Some("Rock".into());
        assert_eq!(format_path("%if{$genre,$genre,Unknown}", &item)?, "Rock");
        Ok(())
    }

    #[test]
    fn nested_function_arguments_are_split_at_the_top_level() -> Result<()> {
        assert_eq!(
            format_path("%if{$genre,%left{2,$genre},Unknown}", &test_item())?,
            "Ro"
        );
        Ok(())
    }

    #[test]
    fn template_validation_checks_every_branch() {
        assert!(validate_template("$artist/%if{$genre,$title,Unknown}").is_ok());
        assert!(validate_template("$artist/%if{$genre,$titel,Unknown}").is_err());
        assert!(validate_template("%left{x,$title}").is_err());
        assert!(validate_template("%if{$genre}").is_err());
        assert!(validate_template("$artist}").is_err());
        assert!(format_path("$artist}", &test_item()).is_err());
    }
}
