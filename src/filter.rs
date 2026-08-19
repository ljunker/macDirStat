use std::{path::Path, time::SystemTime};

use anyhow::{Result, bail};

use crate::{
    analytics::{FileCategory, FileRecord, category_for_extension, now_seconds},
    tree::{Node, NodeKind, SizeMode},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ComparisonOperator {
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
    Equal,
}

impl ComparisonOperator {
    fn matches(self, left: u64, right: u64) -> bool {
        match self {
            Self::Greater => left > right,
            Self::GreaterEqual => left >= right,
            Self::Less => left < right,
            Self::LessEqual => left <= right,
            Self::Equal => left == right,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Predicate {
    Name(String),
    Size(ComparisonOperator, u64),
    Age(ComparisonOperator, u64),
    Extension(Option<String>),
    Category(FileCategory),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FilterExpression {
    predicates: Vec<Predicate>,
}

impl FilterExpression {
    pub fn parse(input: &str) -> Result<Option<Self>> {
        let tokens = tokenize(input)?;
        if tokens.is_empty() {
            return Ok(None);
        }
        let predicates = tokens
            .into_iter()
            .map(|token| parse_predicate(&token))
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(Self { predicates }))
    }

    pub fn matches_file(&self, file: &FileRecord, size_mode: SizeMode) -> bool {
        self.predicates.iter().all(|predicate| match predicate {
            Predicate::Name(query) => file.name.to_string_lossy().to_lowercase().contains(query),
            Predicate::Size(operator, size) => operator.matches(file.usage.size(size_mode), *size),
            Predicate::Age(operator, age) => {
                file_age(file.modified_seconds).is_some_and(|value| operator.matches(value, *age))
            }
            Predicate::Extension(extension) => file.extension.as_ref() == extension.as_ref(),
            Predicate::Category(category) => file.category == *category,
        })
    }

    pub fn matches_node(&self, node: &Node, size_mode: SizeMode) -> bool {
        self.predicates.iter().all(|predicate| match predicate {
            Predicate::Name(query) => node.name.to_string_lossy().to_lowercase().contains(query),
            Predicate::Size(operator, size) => node
                .usage
                .is_some_and(|usage| operator.matches(usage.size(size_mode), *size)),
            Predicate::Age(operator, age) => node
                .modified
                .and_then(system_time_seconds)
                .and_then(file_age)
                .is_some_and(|value| operator.matches(value, *age)),
            Predicate::Extension(extension) => {
                node.kind == NodeKind::File
                    && normalized_extension(&node.path).as_ref() == extension.as_ref()
            }
            Predicate::Category(category) => {
                node.kind == NodeKind::File
                    && category_for_extension(normalized_extension(&node.path).as_deref())
                        == *category
            }
        })
    }

    pub fn matches_path_usage(
        &self,
        path: &Path,
        usage: crate::tree::UsageStats,
        modified_seconds: Option<i64>,
        size_mode: SizeMode,
    ) -> bool {
        self.predicates.iter().all(|predicate| match predicate {
            Predicate::Name(query) => path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .to_lowercase()
                .contains(query),
            Predicate::Size(operator, size) => operator.matches(usage.size(size_mode), *size),
            Predicate::Age(operator, age) => modified_seconds
                .and_then(file_age)
                .is_some_and(|value| operator.matches(value, *age)),
            Predicate::Extension(extension) => {
                normalized_extension(path).as_ref() == extension.as_ref()
            }
            Predicate::Category(category) => {
                category_for_extension(normalized_extension(path).as_deref()) == *category
            }
        })
    }
}

fn parse_predicate(token: &str) -> Result<Predicate> {
    let lower = token.to_lowercase();
    if let Some(value) = lower.strip_prefix("ext:") {
        if value.is_empty() {
            bail!("ext: requires an extension or ext:none");
        }
        return Ok(Predicate::Extension(
            (value != "none" && value != "<none>")
                .then(|| value.trim_start_matches('.').to_owned()),
        ));
    }
    if let Some(value) = lower.strip_prefix("type:") {
        let Some(category) = FileCategory::parse(value) else {
            bail!(
                "unknown type {value:?}; use image, video, audio, archive, document, code or other"
            );
        };
        return Ok(Predicate::Category(category));
    }
    if let Some(rest) = lower.strip_prefix("size") {
        let (operator, value) = parse_comparison(rest, "size")?;
        return Ok(Predicate::Size(operator, parse_size(value)?));
    }
    if let Some(rest) = lower.strip_prefix("age") {
        let (operator, value) = parse_comparison(rest, "age")?;
        return Ok(Predicate::Age(operator, parse_duration(value)?));
    }
    if lower.starts_with(['>', '<', '=']) {
        let (operator, value) = parse_comparison(&lower, "size")?;
        return Ok(Predicate::Size(operator, parse_size(value)?));
    }
    Ok(Predicate::Name(lower))
}

fn parse_comparison<'a>(value: &'a str, label: &str) -> Result<(ComparisonOperator, &'a str)> {
    let (operator, remainder) = if let Some(value) = value.strip_prefix(">=") {
        (ComparisonOperator::GreaterEqual, value)
    } else if let Some(value) = value.strip_prefix("<=") {
        (ComparisonOperator::LessEqual, value)
    } else if let Some(value) = value.strip_prefix('>') {
        (ComparisonOperator::Greater, value)
    } else if let Some(value) = value.strip_prefix('<') {
        (ComparisonOperator::Less, value)
    } else if let Some(value) = value.strip_prefix('=') {
        (ComparisonOperator::Equal, value)
    } else {
        bail!("{label} requires one of >, >=, <, <= or =");
    };
    if remainder.is_empty() {
        bail!("{label} comparison requires a value");
    }
    Ok((operator, remainder))
}

fn parse_size(value: &str) -> Result<u64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid size number {number:?}"))?;
    if !number.is_finite() || number < 0.0 {
        bail!("size must be a finite positive number");
    }
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1_f64,
        "kb" => 1_000_f64,
        "mb" => 1_000_000_f64,
        "gb" => 1_000_000_000_f64,
        "tb" => 1_000_000_000_000_f64,
        "kib" => 1_024_f64,
        "mib" => 1_048_576_f64,
        "gib" => 1_073_741_824_f64,
        "tib" => 1_099_511_627_776_f64,
        _ => bail!("unknown size unit {unit:?}"),
    };
    let bytes = number * multiplier;
    if bytes > u64::MAX as f64 {
        bail!("size is too large");
    }
    Ok(bytes.round() as u64)
}

fn parse_duration(value: &str) -> Result<u64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid age number {number:?}"))?;
    let multiplier = match unit {
        "s" => 1_f64,
        "m" => 60_f64,
        "h" => 3_600_f64,
        "d" => 86_400_f64,
        "w" => 604_800_f64,
        "y" => 31_536_000_f64,
        _ => bail!("unknown age unit {unit:?}; use s, m, h, d, w or y"),
    };
    let seconds = number * multiplier;
    if !seconds.is_finite() || seconds < 0.0 || seconds > u64::MAX as f64 {
        bail!("age must be a finite positive duration");
    }
    Ok(seconds.round() as u64)
}

fn tokenize(input: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for character in input.chars() {
        match (quote, character) {
            (Some(expected), value) if value == expected => quote = None,
            (Some(_), value) => current.push(value),
            (None, '\'' | '"') => quote = Some(character),
            (None, value) if value.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            (None, value) => current.push(value),
        }
    }
    if quote.is_some() {
        bail!("unterminated quote in filter");
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn file_age(modified_seconds: i64) -> Option<u64> {
    let modified = u64::try_from(modified_seconds).ok()?;
    Some(now_seconds().saturating_sub(modified))
}

fn system_time_seconds(time: SystemTime) -> Option<i64> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
}

fn normalized_extension(path: &Path) -> Option<String> {
    path.extension()
        .filter(|extension| !extension.is_empty())
        .map(|extension| extension.to_string_lossy().to_lowercase())
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use super::*;
    use crate::{
        analytics::FileRecord,
        tree::{FileIdentity, UsageStats},
    };

    fn file(name: &str, logical: u64, extension: Option<&str>) -> FileRecord {
        FileRecord {
            path: PathBuf::from("/tmp").join(name),
            name: OsString::from(name),
            usage: UsageStats {
                logical,
                physical: logical / 2,
                files: 1,
            },
            identity: FileIdentity {
                device: 1,
                inode: 2,
                modified_seconds: 1,
                modified_nanoseconds: 0,
            },
            modified_seconds: 1,
            modified_nanoseconds: 0,
            extension: extension.map(str::to_owned),
            category: category_for_extension(extension),
        }
    }

    #[test]
    fn parses_and_applies_combined_predicates() {
        let filter = FilterExpression::parse("photo size>1MiB ext:jpg type:image")
            .unwrap()
            .unwrap();
        assert!(filter.matches_file(
            &file("holiday-photo.jpg", 2 * 1024 * 1024, Some("jpg")),
            SizeMode::Logical
        ));
        assert!(!filter.matches_file(
            &file("holiday-photo.jpg", 500, Some("jpg")),
            SizeMode::Logical
        ));
    }

    #[test]
    fn supports_quotes_shorthand_units_and_no_extension() {
        let filter = FilterExpression::parse("\"large file\" >1.5GB ext:none")
            .unwrap()
            .unwrap();
        assert!(filter.matches_file(&file("large file", 1_600_000_000, None), SizeMode::Logical));
    }

    #[test]
    fn rejects_invalid_filters_without_guessing() {
        assert!(FilterExpression::parse("size1GB").is_err());
        assert!(FilterExpression::parse("age>12parsecs").is_err());
        assert!(FilterExpression::parse("type:unknown").is_err());
        assert!(FilterExpression::parse("\"unterminated").is_err());
    }
}
