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
    let mut result = String::new();
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '$' => {
                let var = collect_identifier(&mut chars);
                let value = get_variable(&var, item)?;
                result.push_str(&sanitize(&value));
            }
            '%' => {
                let func = collect_identifier(&mut chars);
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let arg = collect_until_close(&mut chars)?;
                    let value = apply_function(&func, &arg, item)?;
                    result.push_str(&sanitize(&value));
                } else {
                    return Err(Error::PathFormat(format!("Expected '{{' after %{func}")));
                }
            }
            _ => result.push(c),
        }
    }

    Ok(result)
}

/// Format and validate a relative destination path.
pub fn format_relative_path(template: &str, item: &Item) -> Result<PathBuf> {
    let formatted = format_path(template, item)?;
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

fn apply_function(func: &str, arg: &str, item: &Item) -> Result<String> {
    let expanded = format_path(arg, item)?;

    Ok(match func {
        "upper" => expanded.to_uppercase(),
        "lower" => expanded.to_lowercase(),
        "title" => to_title_case(&expanded),
        "left" => {
            if let Some((n, rest)) = arg.split_once(',') {
                let n: usize = n
                    .parse()
                    .map_err(|e| Error::PathFormat(format!("Invalid number: {e}")))?;
                let val = format_path(rest.trim(), item)?;
                val.chars().take(n).collect()
            } else {
                expanded
            }
        }
        "right" => {
            if let Some((n, rest)) = arg.split_once(',') {
                let n: usize = n
                    .parse()
                    .map_err(|e| Error::PathFormat(format!("Invalid number: {e}")))?;
                let val = format_path(rest.trim(), item)?;
                let len = val.chars().count();
                val.chars().skip(len.saturating_sub(n)).collect()
            } else {
                expanded
            }
        }
        "if" => {
            let parts: Vec<&str> = arg.splitn(3, ',').collect();
            if parts.len() >= 2 {
                let condition = format_path(parts[0].trim(), item)?;
                if !condition.is_empty() {
                    format_path(parts[1].trim(), item)?
                } else if parts.len() == 3 {
                    format_path(parts[2].trim(), item)?
                } else {
                    String::new()
                }
            } else {
                expanded
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

fn sanitize(s: &str) -> String {
    let sanitized = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized == "." || sanitized == ".." || sanitized.is_empty() {
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
    fn rejects_unclosed_functions() {
        assert!(format_path("%upper{$artist", &test_item()).is_err());
    }
}
