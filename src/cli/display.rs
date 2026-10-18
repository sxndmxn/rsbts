use rsbts::{Album, Error, FlexibleValue, Item, Result};

pub(super) const ITEM_FIELDS: &[&str] = &[
    "id",
    "album_id",
    "path",
    "title",
    "artist",
    "album",
    "albumartist",
    "genre",
    "year",
    "track",
    "disc",
    "format",
    "bitrate",
    "length",
    "file_size",
    "added",
    "mtime",
    "singleton",
];
pub(super) const ALBUM_FIELDS: &[&str] =
    &["id", "album", "albumartist", "year", "artpath", "added"];

pub(super) struct DisplayTemplate {
    parts: Vec<Part>,
}

enum Part {
    Text(String),
    Field { name: String, original: String },
}

impl DisplayTemplate {
    pub(super) fn parse(input: &str) -> Result<Self> {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut characters = input.chars().peekable();
        while let Some(character) = characters.next() {
            if character != '$' {
                text.push(character);
                continue;
            }
            if characters.peek() == Some(&'$') {
                characters.next();
                text.push('$');
                continue;
            }
            let braced = characters.peek() == Some(&'{');
            let mut name = String::new();
            if braced {
                characters.next();
                loop {
                    match characters.next() {
                        Some('}') => break,
                        Some(value) => name.push(value),
                        None => return Err(Error::Query("unclosed field in list format".into())),
                    }
                }
                if name.is_empty() {
                    return Err(Error::Query("list format field cannot be empty".into()));
                }
            } else {
                while characters
                    .peek()
                    .is_some_and(|value| value.is_ascii_alphanumeric() || *value == '_')
                {
                    if let Some(value) = characters.next() {
                        name.push(value);
                    }
                }
            }
            if name.is_empty() {
                text.push('$');
                continue;
            }
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            let original = if braced {
                format!("${{{name}}}")
            } else {
                format!("${name}")
            };
            parts.push(Part::Field { name, original });
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Ok(Self { parts })
    }

    pub(super) fn item(&self, item: &Item) -> String {
        self.render(|name| item_value(item, name))
    }

    pub(super) fn album(&self, album: &Album) -> String {
        self.render(|name| match name {
            "id" => Some(optional(album.id)),
            "album" => Some(album.album.clone()),
            "albumartist" => Some(album.albumartist.clone()),
            "year" => Some(optional(album.year)),
            "artpath" => Some(
                album
                    .artpath
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            ),
            "added" => Some(album.added.to_rfc3339()),
            _ => flexible_value(&album.extended.flexible_fields, name),
        })
    }

    fn render(&self, value: impl Fn(&str) -> Option<String>) -> String {
        let mut output = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => output.push_str(text),
                Part::Field { name, original } => {
                    output.push_str(value(name).as_deref().unwrap_or(original));
                }
            }
        }
        output
    }
}

fn optional(value: Option<impl std::fmt::Display>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn item_value(item: &Item, name: &str) -> Option<String> {
    Some(match name {
        "id" => optional(item.id),
        "album_id" => optional(item.album_id),
        "path" => item.path.display().to_string(),
        "title" => item.title.clone(),
        "artist" => item.artist.clone(),
        "album" => item.album.clone(),
        "albumartist" => item.effective_albumartist().to_string(),
        "genre" => item.genre.clone().unwrap_or_default(),
        "year" => optional(item.year),
        "track" => item
            .track
            .map(|value| format!("{value:02}"))
            .unwrap_or_default(),
        "disc" => optional(item.disc),
        "format" => item.format.as_str().into(),
        "bitrate" => item.bitrate.to_string(),
        "length" => item.length.to_string(),
        "file_size" => optional(item.file_size),
        "added" => item.added.to_rfc3339(),
        "mtime" => item.mtime.to_rfc3339(),
        "singleton" => u8::from(item.singleton).to_string(),
        _ => return flexible_value(&item.extended.flexible_fields, name),
    })
}

fn flexible_value(
    fields: &std::collections::BTreeMap<String, FlexibleValue>,
    name: &str,
) -> Option<String> {
    let value = fields.get(name.strip_prefix("flex.").unwrap_or(name))?;
    Some(match value {
        FlexibleValue::String(value) => value.clone(),
        FlexibleValue::Integer(value) => value.to_string(),
        FlexibleValue::Float(value) => value.to_string(),
        FlexibleValue::Boolean(value) => u8::from(*value).to_string(),
        FlexibleValue::Date(value) => match (value.year, value.month, value.day) {
            (Some(year), Some(month), Some(day)) => format!("{year:04}-{month:02}-{day:02}"),
            (Some(year), Some(month), _) => format!("{year:04}-{month:02}"),
            (Some(year), _, _) => format!("{year:04}"),
            _ => String::new(),
        },
        FlexibleValue::StringList(values) => values.join("; "),
        _ => return None,
    })
}
