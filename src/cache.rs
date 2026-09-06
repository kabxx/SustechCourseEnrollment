use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Semester {
    pub p_xn: Value,
    pub p_xq: Value,
    pub p_xnxq: Value,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Semester {
    pub fn from_json(value: &Value) -> Result<Self> {
        let Some(object) = value.as_object() else {
            return Err(AppError::Course(
                "semester response did not contain p_xn/p_xq/p_xnxq".into(),
            ));
        };

        let get = |name: &str| {
            object
                .get(name)
                .cloned()
                .ok_or_else(|| AppError::Course(format!("semester response missing {name}")))
        };
        Ok(Self {
            p_xn: get("p_xn")?,
            p_xq: get("p_xq")?,
            p_xnxq: get("p_xnxq")?,
            extra: object
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "p_xn" | "p_xq" | "p_xnxq"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        })
    }

    pub fn form_value(value: &Value) -> String {
        match value {
            Value::String(value) => value.clone(),
            Value::Number(value) => value.to_string(),
            Value::Bool(value) => value.to_string(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }

    pub fn form_fields(&self) -> [(&'static str, String); 3] {
        [
            ("p_xn", Self::form_value(&self.p_xn)),
            ("p_xq", Self::form_value(&self.p_xq)),
            ("p_xnxq", Self::form_value(&self.p_xnxq)),
        ]
    }

    pub fn same_term(&self, other: &Self) -> bool {
        self.form_fields()
            .into_iter()
            .zip(other.form_fields())
            .all(|((_, left), (_, right))| left == right)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Course {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub code: Option<String>,
    pub class_name: Option<String>,
    pub xkms: Option<String>,
    pub xkxs: Option<String>,
    pub jfxs: Option<String>,
    pub schedule: Vec<CourseTime>,
    pub details: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CourseTime {
    pub weekday: Option<u8>,
    pub start_section: Option<u16>,
    pub end_section: Option<u16>,
    pub weeks: Option<String>,
}

impl CourseTime {
    pub fn is_complete(&self) -> bool {
        self.weekday.is_some()
            && self.start_section.is_some()
            && self.end_section.is_some()
            && self
                .weeks
                .as_deref()
                .is_some_and(|weeks| !weeks.trim().is_empty())
    }

    pub fn overlaps(&self, other: &Self) -> Option<bool> {
        let (Some(day), Some(other_day)) = (self.weekday, other.weekday) else {
            return None;
        };
        if day != other_day {
            return Some(false);
        }
        let (Some(start), Some(end), Some(other_start), Some(other_end)) = (
            self.start_section,
            self.end_section,
            other.start_section,
            other.end_section,
        ) else {
            return None;
        };
        if end < start || other_end < other_start {
            return None;
        }
        let periods_overlap = start <= other_end && other_start <= end;
        if !periods_overlap {
            return Some(false);
        }
        match (&self.weeks, &other.weeks) {
            (Some(left), Some(right)) if !left.trim().is_empty() && !right.trim().is_empty() => {
                Some(weeks_may_overlap(left, right))
            }
            _ => None,
        }
    }
}

fn weeks_may_overlap(left: &str, right: &str) -> bool {
    let left = parse_week_set(left);
    let right = parse_week_set(right);
    match (left, right) {
        (Some(left), Some(right)) => left.iter().any(|week| right.contains(week)),
        _ => true,
    }
}

fn parse_week_set(value: &str) -> Option<std::collections::BTreeSet<u16>> {
    let lower = value.to_ascii_lowercase();
    let parity = if lower.contains("单周") || lower.contains("odd") {
        Some(1)
    } else if lower.contains("双周") || lower.contains("even") {
        Some(0)
    } else {
        None
    };

    let normalized = value
        .chars()
        .map(|character| match character {
            '，' | '、' | '；' | ';' | '/' | '|' => ',',
            '至' | '到' | '~' | '～' | '–' | '—' | '−' | '－' => '-',
            character => character,
        })
        .collect::<String>();

    let mut set = std::collections::BTreeSet::new();
    let mut parsed_any = false;
    for segment in normalized.split(',') {
        for token in segment.split_whitespace() {
            let numbers = match numeric_runs(token) {
                Ok(numbers) => numbers,
                Err(()) => return None,
            };
            if numbers.is_empty() {
                continue;
            }
            if token.contains('-') {
                if numbers.len() != 2 {
                    return None;
                }
                let start = numbers[0];
                let end = numbers[1];
                if start == 0 || end == 0 || end < start || end - start > 64 {
                    return None;
                }
                set.extend(start..=end);
                parsed_any = true;
            } else {
                for number in numbers {
                    if number == 0 {
                        return None;
                    }
                    set.insert(number);
                    parsed_any = true;
                }
            }
        }
    }
    if let Some(parity) = parity {
        set.retain(|week| week % 2 == parity);
    }
    (parsed_any && !set.is_empty()).then_some(set)
}

fn numeric_runs(value: &str) -> std::result::Result<Vec<u16>, ()> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for character in value.chars() {
        if character.is_ascii_digit() {
            current.push(character);
        } else if !current.is_empty() {
            if let Ok(number) = current.parse::<u16>() {
                runs.push(number);
            } else {
                return Err(());
            }
            current.clear();
        }
    }
    if !current.is_empty() {
        if let Ok(number) = current.parse::<u16>() {
            runs.push(number);
        } else {
            return Err(());
        }
    }
    Ok(runs)
}

impl Course {
    pub fn display_code(&self) -> &str {
        self.code.as_deref().unwrap_or("")
    }

    pub fn display_name(&self) -> String {
        match self
            .class_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(class_name) => format!("{} · {}", self.name, class_name),
            None => self.name.clone(),
        }
    }

    pub fn has_schedule_data(&self) -> bool {
        !self.schedule.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CacheFile {
    pub version: u32,
    pub id: String,
    pub training_type: String,
    pub semester: Semester,
    pub course_kinds: Vec<String>,
    pub courses: BTreeMap<String, Course>,
    pub selected_courses: Option<BTreeMap<String, Course>>,
    pub updated_unix: u64,
}

impl CacheFile {
    pub fn new(
        id: String,
        training_type: String,
        semester: Semester,
        course_kinds: Vec<String>,
        courses: BTreeMap<String, Course>,
    ) -> Self {
        let updated_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        Self {
            version: CACHE_VERSION,
            id,
            training_type,
            semester,
            course_kinds,
            courses,
            selected_courses: None,
            updated_unix,
        }
    }

    pub fn with_selected_courses(mut self, selected_courses: BTreeMap<String, Course>) -> Self {
        self.selected_courses = Some(selected_courses);
        self
    }

    pub fn usable_offline(&self, id: &str, training_type: &str) -> bool {
        self.version == CACHE_VERSION
            && self.id == id
            && self.training_type == training_type
            && !self.semester.form_fields()[2].1.trim().is_empty()
            && !self.course_kinds.is_empty()
            && !self.courses.is_empty()
    }
}

pub fn load(path: &Path) -> Result<Option<CacheFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)
        .map_err(|error| AppError::Cache(format!("read {}: {error}", path.display())))?;
    let cache = serde_json::from_str(&text)
        .map_err(|error| AppError::Cache(format!("parse {}: {error}", path.display())))?;
    Ok(Some(cache))
}

pub fn save(path: &Path, cache: &CacheFile) -> Result<()> {
    let parent = path.parent().filter(|value| !value.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("json.tmp");
    let text = serde_json::to_vec_pretty(cache)?;
    fs::write(&temp_path, text)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn parses_semester_and_preserves_extra_fields() {
        let semester = Semester::from_json(&json!({
            "p_xn": 2025,
            "p_xq": "秋",
            "p_xnxq": "2025-2026-1",
            "label": "current"
        }))
        .unwrap();
        assert_eq!(Semester::form_value(&semester.p_xn), "2025");
        assert_eq!(semester.extra["label"], json!("current"));
    }

    #[test]
    fn cache_round_trip_and_offline_check() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let semester = Semester::from_json(&json!({
            "p_xn": "2025", "p_xq": "1", "p_xnxq": "2025-1"
        }))
        .unwrap();
        let courses = BTreeMap::from([(
            "row-1".to_owned(),
            Course {
                id: "row-1".into(),
                name: "课程".into(),
                kind: "jhnxk".into(),
                xkms: Some("1".into()),
                ..Course::default()
            },
        )]);
        let cache = CacheFile::new(
            "1".into(),
            "2".into(),
            semester.clone(),
            vec!["jhnxk".into()],
            courses,
        );
        save(&path, &cache).unwrap();
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.version, CACHE_VERSION);
        assert!(loaded.usable_offline("1", "2"));
        assert!(!loaded.usable_offline("1", "1"));
    }

    #[test]
    fn course_time_overlap_is_conservative() {
        let first = CourseTime {
            weekday: Some(2),
            start_section: Some(3),
            end_section: Some(4),
            weeks: Some("1-16周".into()),
        };
        let second = CourseTime {
            weekday: Some(2),
            start_section: Some(4),
            end_section: Some(5),
            weeks: Some("8-20周".into()),
        };
        assert_eq!(first.overlaps(&second), Some(true));

        let different_day = CourseTime {
            weekday: Some(3),
            ..first.clone()
        };
        assert_eq!(first.overlaps(&different_day), Some(false));

        let unknown = CourseTime {
            weeks: None,
            ..first
        };
        assert_eq!(unknown.overlaps(&second), None);
    }

    #[test]
    fn parses_week_lists_and_chinese_range_separators() {
        let odd_weeks = parse_week_set("1、3、5周").unwrap();
        assert_eq!(odd_weeks.into_iter().collect::<Vec<_>>(), vec![1, 3, 5]);

        let range = parse_week_set("第1至8周").unwrap();
        assert_eq!(range.len(), 8);
        assert!(range.contains(&1) && range.contains(&8));

        let mixed = parse_week_set("1-4, 7～9周").unwrap();
        assert_eq!(mixed.len(), 7);
        assert!(mixed.contains(&1) && mixed.contains(&8) && mixed.contains(&9));
    }

    #[test]
    fn week_list_overlap_does_not_pair_adjacent_singletons_as_a_range() {
        let first = CourseTime {
            weekday: Some(2),
            start_section: Some(3),
            end_section: Some(4),
            weeks: Some("1,3,5周".into()),
        };
        let disjoint = CourseTime {
            weeks: Some("2,4周".into()),
            ..first.clone()
        };
        assert_eq!(first.overlaps(&disjoint), Some(false));

        let overlapping = CourseTime {
            weeks: Some("3,6周".into()),
            ..first.clone()
        };
        assert_eq!(first.overlaps(&overlapping), Some(true));
    }

    #[test]
    fn parity_week_expressions_remain_conservative() {
        assert!(parse_week_set("单双周").is_none());
        assert!(parse_week_set("1-999999999999周").is_none());
        let first = CourseTime {
            weekday: Some(1),
            start_section: Some(1),
            end_section: Some(2),
            weeks: Some("单周".into()),
        };
        let second = CourseTime {
            weeks: Some("2-8周".into()),
            ..first.clone()
        };
        assert_eq!(first.overlaps(&second), Some(true));
    }

    #[test]
    fn course_display_code_is_optional_in_current_rows() {
        let course: Course = serde_json::from_value(json!({
            "id": "row-1", "name": "课程", "kind": "jhnxk"
        }))
        .unwrap();
        assert_eq!(course.display_code(), "");
        assert!(!course.has_schedule_data());
    }
}
