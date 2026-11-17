//! Documents of kind `decision`: who decided, when and where. The facts live in the document's
//! `metadata` JSON (no extra column). [`validate`] checks them when a document is stored,
//! [`Decision::from_metadata`] reads them back leniently for citations and search hits, and
//! [`Decision::label`] is the text that goes in front of a decision's passage in a prompt.

use chrono::{DateTime, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The document kind that carries decision metadata.
pub const KIND: &str = "decision";

/// The values of `where`: a folder in a repo, a factory project, or entered by hand.
pub const WHERE_VALUES: [&str; 3] = ["repo_folder", "factory_project", "by_hand"];

/// Names per list and bytes per name or text field.
const MAX_NAMES: usize = 50;
const MAX_NAME_BYTES: usize = 256;
const MAX_REF_BYTES: usize = 2048;

/// The decision metadata of a document, as a citation or a search hit carries it. Absent fields
/// are left out of the JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decided_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
    #[serde(rename = "where", default, skip_serializing_if = "Option::is_none")]
    pub r#where: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
}

impl Decision {
    /// The decision fields of a stored `metadata` object. Fields of the wrong type, blank
    /// strings and blank names are skipped; this never fails, because a read must not break on
    /// what was stored. `None` when the metadata is not an object.
    pub fn from_metadata(metadata: &Value) -> Option<Decision> {
        let map = metadata.as_object()?;
        Some(Decision {
            decided_by: names(map.get("decided_by")),
            decided_at: text(map.get("decided_at")),
            changed_by: names(map.get("changed_by")),
            changed_at: text(map.get("changed_at")),
            pr: text(map.get("pr")),
            r#where: text(map.get("where")),
            repo: text(map.get("repo")),
            project: text(map.get("project")),
            added_by: text(map.get("added_by")),
        })
    }

    /// The decision fields of a stored `metadata` object, or `None` when there are none to
    /// show (not an object, or nothing usable in it).
    pub fn usable(metadata: &Value) -> Option<Decision> {
        Self::from_metadata(metadata).filter(|d| *d != Decision::default())
    }

    /// What goes in a prompt in place of the kind: `decision, decided by Ana and Bo on 3 August
    /// 2026`. A missing decider or date is left out, never printed as a placeholder.
    pub fn label(&self) -> String {
        let mut out = KIND.to_string();
        let date = self.decided_at.as_deref().map(human_date);
        match (self.decided_by.is_empty(), date) {
            (false, Some(date)) => {
                out.push_str(&format!(
                    ", decided by {} on {date}",
                    join_names(&self.decided_by)
                ));
            }
            (false, None) => {
                out.push_str(&format!(", decided by {}", join_names(&self.decided_by)))
            }
            (true, Some(date)) => out.push_str(&format!(", decided on {date}")),
            (true, None) => {}
        }
        out
    }
}

/// A string field, trimmed; `None` when it is missing, not a string or blank.
fn text(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// The non-blank strings of an array. A lone string counts as one name.
fn names(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| text(Some(i)))
            .collect::<Vec<_>>(),
        Some(single @ Value::String(_)) => text(Some(single)).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// `Ana`, `Ana and Bo`, `Ana, Bo and Cy`.
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// `2026-08-03` and `2026-08-03T09:30:00+02:00` as `3 August 2026` (the date as written, with no
/// change of time zone). Anything else is shown as stored.
fn human_date(s: &str) -> String {
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.format("%-d %B %Y").to_string();
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return t.date_naive().format("%-d %B %Y").to_string();
    }
    s.to_string()
}

/// True for `YYYY-MM-DD` and for an RFC 3339 timestamp.
fn is_date_or_timestamp(s: &str) -> bool {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() || DateTime::parse_from_rfc3339(s).is_ok()
}

/// Checks the metadata of a document of kind `decision`.
///
/// Required: `decided_by` (a list of 1 to 50 non-empty names) and `decided_at` (`YYYY-MM-DD` or an
/// RFC 3339 timestamp). Optional, and `null` counts as absent: `changed_by` (a list of names, may
/// be empty), `changed_at` (a date like `decided_at`), `pr`, `repo`, `project` and `added_by`
/// (non-empty strings) and `where` (`repo_folder`, `factory_project` or `by_hand`). Other keys
/// are left alone. The error names the offending field as `metadata.<field>`.
pub fn validate(metadata: &Map<String, Value>) -> Result<(), String> {
    let present = |field: &str| metadata.get(field).filter(|v| !v.is_null());

    match present("decided_by") {
        None => return Err("metadata.decided_by is required for a decision".to_string()),
        Some(v) => check_names("decided_by", v, 1)?,
    }
    match present("decided_at") {
        None => return Err("metadata.decided_at is required for a decision".to_string()),
        Some(v) => check_date("decided_at", v)?,
    }
    if let Some(v) = present("changed_by") {
        check_names("changed_by", v, 0)?;
    }
    if let Some(v) = present("changed_at") {
        check_date("changed_at", v)?;
    }
    for (field, max) in [
        ("pr", MAX_REF_BYTES),
        ("repo", MAX_NAME_BYTES),
        ("project", MAX_NAME_BYTES),
        ("added_by", MAX_NAME_BYTES),
    ] {
        if let Some(v) = present(field) {
            check_string(field, v, max)?;
        }
    }
    if let Some(v) = present("where") {
        match v.as_str() {
            Some(w) if WHERE_VALUES.contains(&w) => {}
            _ => {
                return Err(format!(
                    "metadata.where must be one of {}",
                    WHERE_VALUES.join(", ")
                ));
            }
        }
    }
    Ok(())
}

fn check_string(field: &str, v: &Value, max: usize) -> Result<(), String> {
    match v.as_str() {
        Some(s) if !s.trim().is_empty() && s.len() <= max => Ok(()),
        Some(s) if s.len() > max => Err(format!("metadata.{field} is over {max} bytes")),
        _ => Err(format!("metadata.{field} must be a non-empty string")),
    }
}

fn check_names(field: &str, v: &Value, min: usize) -> Result<(), String> {
    let Some(items) = v.as_array() else {
        return Err(format!(
            "metadata.{field} must be an array of non-empty strings"
        ));
    };
    if items.len() < min || items.len() > MAX_NAMES {
        return Err(format!(
            "metadata.{field} must have {min} to {MAX_NAMES} names"
        ));
    }
    for item in items {
        match item.as_str() {
            Some(s) if !s.trim().is_empty() && s.len() <= MAX_NAME_BYTES => {}
            _ => {
                return Err(format!(
                    "metadata.{field} must be an array of non-empty strings of at most \
                     {MAX_NAME_BYTES} bytes"
                ));
            }
        }
    }
    Ok(())
}

fn check_date(field: &str, v: &Value) -> Result<(), String> {
    match v.as_str() {
        Some(s) if is_date_or_timestamp(s) => Ok(()),
        _ => Err(format!(
            "metadata.{field} must be a date (YYYY-MM-DD) or an RFC 3339 timestamp"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(v: Value) -> Result<(), String> {
        validate(v.as_object().unwrap())
    }

    fn valid() -> Value {
        json!({"decided_by": ["Ashish Bagri"], "decided_at": "2026-08-03"})
    }

    #[test]
    fn the_minimum_is_decided_by_and_decided_at() {
        assert_eq!(check(valid()), Ok(()));
        let err = check(json!({"decided_at": "2026-08-03"})).unwrap_err();
        assert!(err.contains("decided_by is required"), "{err}");
        let err = check(json!({"decided_by": ["A"]})).unwrap_err();
        assert!(err.contains("decided_at is required"), "{err}");
        // null is the same as missing.
        let err = check(json!({"decided_by": null, "decided_at": "2026-08-03"})).unwrap_err();
        assert!(err.contains("decided_by is required"), "{err}");
    }

    #[test]
    fn every_field_is_accepted_together_and_other_keys_are_left_alone() {
        let all = json!({
            "decided_by": ["Ana", "Bo"], "decided_at": "2026-08-03T09:30:00+02:00",
            "changed_by": ["Cy"], "changed_at": "2026-09-01",
            "pr": "https://github.com/glassflow/lore/pull/42", "where": "repo_folder",
            "repo": "glassflow/lore", "project": "TR-1208", "added_by": "tares",
            "path": "decisions/0001.md", "extra": {"x": 1},
        });
        assert_eq!(check(all), Ok(()));
        // Optional fields may be null.
        let nulls = json!({
            "decided_by": ["A"], "decided_at": "2026-08-03", "changed_by": null,
            "changed_at": null, "pr": null, "where": null, "repo": null, "project": null,
            "added_by": null,
        });
        assert_eq!(check(nulls), Ok(()));
        assert_eq!(
            check(json!({"decided_by": ["A"], "decided_at": "2026-08-03", "changed_by": []})),
            Ok(())
        );
    }

    #[test]
    fn decided_by_must_be_a_list_of_non_empty_names() {
        for bad in [
            json!("Ana"),
            json!([]),
            json!([""]),
            json!(["  "]),
            json!(["Ana", 3]),
            json!([["Ana"]]),
            json!({"name": "Ana"}),
            json!(7),
            json!(["n".repeat(MAX_NAME_BYTES + 1)]),
            json!(vec!["n"; MAX_NAMES + 1]),
        ] {
            let err = check(json!({"decided_by": bad, "decided_at": "2026-08-03"})).unwrap_err();
            assert!(err.starts_with("metadata.decided_by"), "{bad}: {err}");
        }
        let err = check(json!({"decided_by": "Ana", "decided_at": "2026-08-03"})).unwrap_err();
        assert!(err.contains("array"), "{err}");
    }

    #[test]
    fn changed_by_must_be_a_list_of_non_empty_names() {
        for bad in [json!("Ana"), json!([""]), json!([1])] {
            let mut m = valid();
            m["changed_by"] = bad.clone();
            let err = check(m).unwrap_err();
            assert!(err.starts_with("metadata.changed_by"), "{bad}: {err}");
        }
    }

    #[test]
    fn dates_are_a_day_or_an_rfc3339_timestamp() {
        for ok in [
            "2026-08-03",
            "2026-08-03T09:30:00Z",
            "2026-08-03T09:30:00.5+02:00",
        ] {
            let mut m = valid();
            m["decided_at"] = json!(ok);
            m["changed_at"] = json!(ok);
            assert_eq!(check(m), Ok(()), "{ok}");
        }
        for bad in [
            json!("3 August 2026"),
            json!("2026-13-01"),
            json!("2026-08-03 09:30"),
            json!(""),
            json!(20260803),
            json!(true),
        ] {
            let mut m = valid();
            m["decided_at"] = bad.clone();
            let err = check(m).unwrap_err();
            assert!(err.starts_with("metadata.decided_at"), "{bad}: {err}");
            let mut m = valid();
            m["changed_at"] = bad.clone();
            let err = check(m).unwrap_err();
            assert!(err.starts_with("metadata.changed_at"), "{bad}: {err}");
        }
    }

    #[test]
    fn where_is_one_of_three_values() {
        for ok in WHERE_VALUES {
            let mut m = valid();
            m["where"] = json!(ok);
            assert_eq!(check(m), Ok(()), "{ok}");
        }
        for bad in [json!("github"), json!(""), json!("Repo_Folder"), json!(1)] {
            let mut m = valid();
            m["where"] = bad.clone();
            let err = check(m).unwrap_err();
            assert!(
                err.contains("metadata.where")
                    && err.contains("repo_folder, factory_project, by_hand"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn pr_repo_project_and_added_by_are_non_empty_strings() {
        for field in ["pr", "repo", "project", "added_by"] {
            for bad in [json!(""), json!("  "), json!(5), json!(["x"])] {
                let mut m = valid();
                m[field] = bad.clone();
                let err = check(m).unwrap_err();
                assert!(
                    err.starts_with(&format!("metadata.{field}")),
                    "{bad}: {err}"
                );
            }
            let mut m = valid();
            m[field] = json!("x".repeat(MAX_REF_BYTES + 1));
            assert!(check(m).unwrap_err().contains("is over"), "{field}");
        }
    }

    #[test]
    fn from_metadata_reads_the_fields_and_skips_what_is_unusable() {
        let d = Decision::from_metadata(&json!({
            "decided_by": ["Ana", " ", "Bo", 3], "decided_at": " 2026-08-03 ",
            "changed_by": "Cy", "changed_at": 5, "pr": "", "where": "by_hand",
            "repo": "r", "project": null, "added_by": "tares", "path": "x",
        }))
        .unwrap();
        assert_eq!(d.decided_by, ["Ana", "Bo"]);
        assert_eq!(d.decided_at.as_deref(), Some("2026-08-03"));
        assert_eq!(d.changed_by, ["Cy"]);
        assert_eq!(d.changed_at, None);
        assert_eq!(d.pr, None);
        assert_eq!(d.r#where.as_deref(), Some("by_hand"));
        assert_eq!(d.project, None);
        assert_eq!(Decision::from_metadata(&json!([1])), None);
        assert_eq!(
            Decision::from_metadata(&json!({})).unwrap(),
            Decision::default()
        );
    }

    #[test]
    fn usable_is_none_when_nothing_is_left_to_show() {
        assert_eq!(Decision::usable(&json!({})), None);
        assert_eq!(Decision::usable(&json!({"decided_by": ["", " "]})), None);
        assert_eq!(Decision::usable(&json!("x")), None);
        assert!(Decision::usable(&json!({"decided_by": ["Ana"]})).is_some());
    }

    #[test]
    fn serialized_json_has_the_agreed_field_names_and_no_absent_fields() {
        let d = Decision::from_metadata(&json!({
            "decided_by": ["Ana"], "decided_at": "2026-08-03", "where": "repo_folder",
            "pr": "https://x/pull/1",
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({
                "decided_by": ["Ana"], "decided_at": "2026-08-03",
                "pr": "https://x/pull/1", "where": "repo_folder",
            })
        );
    }

    fn label_of(v: Value) -> String {
        Decision::from_metadata(&v).unwrap().label()
    }

    #[test]
    fn the_label_says_who_decided_and_when() {
        assert_eq!(
            label_of(json!({"decided_by": ["Ashish Bagri"], "decided_at": "2026-08-03"})),
            "decision, decided by Ashish Bagri on 3 August 2026"
        );
        assert_eq!(
            label_of(
                json!({"decided_by": ["Ana", "Bo"], "decided_at": "2026-12-25T23:30:00-05:00"})
            ),
            "decision, decided by Ana and Bo on 25 December 2026"
        );
        assert_eq!(
            label_of(json!({"decided_by": ["Ana", "Bo", "Cy"], "decided_at": "2026-01-09"})),
            "decision, decided by Ana, Bo and Cy on 9 January 2026"
        );
    }

    #[test]
    fn the_label_leaves_out_what_is_missing_and_never_prints_none() {
        assert_eq!(
            label_of(json!({"decided_by": ["Ana"]})),
            "decision, decided by Ana"
        );
        assert_eq!(
            label_of(json!({"decided_at": "2026-08-03"})),
            "decision, decided on 3 August 2026"
        );
        assert_eq!(label_of(json!({})), "decision");
        assert_eq!(
            label_of(json!({"decided_by": [], "decided_at": ""})),
            "decision"
        );
        assert_eq!(
            label_of(json!({"decided_by": [null, "", " "], "decided_at": null})),
            "decision"
        );
        // A date that is not one is shown as stored, not dropped.
        assert_eq!(
            label_of(json!({"decided_by": ["Ana"], "decided_at": "last spring"})),
            "decision, decided by Ana on last spring"
        );
        for v in [
            json!({}),
            json!({"decided_by": [null]}),
            json!({"decided_at": 3}),
        ] {
            let l = label_of(v);
            assert!(!l.contains("None") && !l.contains("null"), "{l}");
        }
    }
}
