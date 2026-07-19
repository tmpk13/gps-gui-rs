//! Loadable, editable RADIO.TOML for the WIO-E5 board.
//!
//! Unlike [`crate::config`] (the app's own settings), this file belongs to the
//! firmware: the Radio page reads it, offers a per-field editor, and writes it
//! back keeping the comments and the `<key>_description` help strings intact.
//! `toml_edit` is used precisely so a round-trip preserves everything the file
//! carries; only the edited value changes.
//!
//! Each editable key is rendered with an input matched to its data type. The
//! type is inferred from the TOML value, but a sibling `<key>_type` string can
//! force it, which also lets a string be presented as a dropdown:
//!
//! ```toml
//! power_mode = "full"
//! power_mode_type = "enum:full,psmoo,psmct"   # dropdown of the three choices
//! meas_rate_ms_type = "int"                    # force an integer input
//! ```
//!
//! Valid `<key>_type` values are `int`, `float`, `bool`, `string`, or
//! `enum:a,b,c`. The firmware ignores unknown keys, so both `<key>_description`
//! and `<key>_type` are inert to the board.
//!
//! Saving copies the previous on-disk file into a `radio-backups` directory
//! (next to the file) under a timestamped name before overwriting, so old
//! versions are kept and can be restored.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use toml_edit::{DocumentMut, Item, Table, Value};

/// The input a field is rendered with, inferred from its TOML value or forced
/// by a sibling `<key>_type` string.
#[derive(Clone, PartialEq, Debug)]
pub enum FieldType {
    /// Whole number: a draggable integer input.
    Int,
    /// Real number: a draggable float input.
    Float,
    /// Boolean: a checkbox.
    Bool,
    /// Free text: a single-line text field.
    Str,
    /// A string constrained to a fixed set of choices: a dropdown. Options come
    /// from `<key>_type = "enum:a,b,c"`.
    Enum(Vec<String>),
}

/// One editable setting: where it lives and how to render it. The live value is
/// read from (and written to) the document, so this holds only fixed metadata.
pub struct RadioField {
    /// The `[section]` the key sits under; empty for a top-level key.
    pub section: String,
    pub key: String,
    pub ty: FieldType,
    /// Help text from the sibling `<key>_description` string, if present.
    pub description: Option<String>,
}

/// A value being edited, typed so the editor can bind a type-specific widget to
/// it and write it back without string parsing.
#[derive(Clone)]
pub enum EditVal {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

/// A loaded RADIO.TOML: the parsed document (the source of truth for values),
/// the ordered list of editable fields, and where it came from.
pub struct RadioDoc {
    doc: DocumentMut,
    /// Editable fields in file order, grouped by section.
    pub fields: Vec<RadioField>,
    /// The file this was loaded from and is saved back to.
    pub path: PathBuf,
    /// A value has been edited since the last load/save.
    pub dirty: bool,
}

/// Whether `key` is one of the metadata keys (`<name>_description` /
/// `<name>_type`) rather than an editable setting.
fn is_meta_key(key: &str) -> bool {
    key.ends_with("_description") || key.ends_with("_type")
}

/// Parse a `<key>_type` string into a [`FieldType`], or `None` if unrecognized
/// (in which case the value's own type is used instead).
fn parse_type_spec(spec: &str) -> Option<FieldType> {
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix("enum:") {
        let opts: Vec<String> = rest
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        return (!opts.is_empty()).then_some(FieldType::Enum(opts));
    }
    match spec.to_lowercase().as_str() {
        "int" | "integer" => Some(FieldType::Int),
        "float" | "double" => Some(FieldType::Float),
        "bool" | "boolean" => Some(FieldType::Bool),
        "string" | "str" => Some(FieldType::Str),
        _ => None,
    }
}

/// The field type for `key`: the sibling `<key>_type` override when valid,
/// otherwise inferred from the value itself.
fn field_type(table: &Table, key: &str, val: &Value) -> FieldType {
    if let Some(spec) = table
        .get(&format!("{key}_type"))
        .and_then(Item::as_str)
        .and_then(parse_type_spec)
    {
        return spec;
    }
    match val {
        Value::Integer(_) => FieldType::Int,
        Value::Float(_) => FieldType::Float,
        Value::Boolean(_) => FieldType::Bool,
        _ => FieldType::Str,
    }
}

/// Append the scalar settings of one table to `out`. Nested tables and the
/// metadata keys are skipped.
fn collect_table(section: &str, table: &Table, out: &mut Vec<RadioField>) {
    for (key, item) in table.iter() {
        let Some(val) = item.as_value() else { continue };
        if is_meta_key(key) {
            continue;
        }
        out.push(RadioField {
            section: section.to_string(),
            key: key.to_string(),
            ty: field_type(table, key, val),
            description: table
                .get(&format!("{key}_description"))
                .and_then(Item::as_str)
                .map(str::to_string),
        });
    }
}

/// Build the editable-field list from a document: top-level scalars first, then
/// each `[section]` in file order.
fn collect_fields(doc: &DocumentMut) -> Vec<RadioField> {
    let root = doc.as_table();
    let mut out = Vec::new();
    collect_table("", root, &mut out);
    for (name, item) in root.iter() {
        if let Some(table) = item.as_table() {
            collect_table(name, table, &mut out);
        }
    }
    out
}

impl RadioDoc {
    /// Read and parse the RADIO.TOML at `path`. A returned `Err` is a
    /// human-readable message for the UI.
    pub fn load(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let doc: DocumentMut = text.parse().map_err(|e| format!("{path}: {e}"))?;
        Ok(Self {
            fields: collect_fields(&doc),
            doc,
            path: PathBuf::from(path),
            dirty: false,
        })
    }

    /// The value item at `section`/`key`, if present.
    fn value(&self, section: &str, key: &str) -> Option<&Value> {
        let table = if section.is_empty() {
            self.doc.as_table()
        } else {
            self.doc.get(section)?.as_table()?
        };
        table.get(key)?.as_value()
    }

    /// The current value formatted for read-only display (no surrounding
    /// whitespace or quotes).
    pub fn display_at(&self, section: &str, key: &str) -> String {
        match self.value(section, key) {
            Some(Value::String(s)) => s.value().clone(),
            Some(Value::Integer(i)) => i.value().to_string(),
            Some(Value::Float(f)) => f.value().to_string(),
            Some(Value::Boolean(b)) => b.value().to_string(),
            Some(other) => other.to_string().trim().to_string(),
            None => String::new(),
        }
    }

    /// The field metadata for `section`/`key`, if it is an editable field.
    fn field_at(&self, section: &str, key: &str) -> Option<&RadioField> {
        self.fields
            .iter()
            .find(|f| f.section == section && f.key == key)
    }

    /// Seed an [`EditVal`] from the current value, typed per the field so the
    /// editor binds the matching widget.
    pub fn edit_val_at(&self, section: &str, key: &str) -> EditVal {
        let v = self.value(section, key);
        match self.field_at(section, key).map(|f| &f.ty) {
            Some(FieldType::Int) => EditVal::Int(v.and_then(Value::as_integer).unwrap_or(0)),
            Some(FieldType::Float) => EditVal::Float(v.and_then(Value::as_float).unwrap_or(0.0)),
            Some(FieldType::Bool) => EditVal::Bool(v.and_then(Value::as_bool).unwrap_or(false)),
            _ => EditVal::Str(v.and_then(Value::as_str).unwrap_or_default().to_string()),
        }
    }

    /// Write an edited value back into the document, keeping the value's
    /// original formatting (leading/trailing whitespace). Marks the doc dirty.
    pub fn apply(&mut self, section: &str, key: &str, val: &EditVal) {
        let table = if section.is_empty() {
            Some(self.doc.as_table_mut())
        } else {
            self.doc.get_mut(section).and_then(Item::as_table_mut)
        };
        let Some(item) = table.and_then(|t| t.get_mut(key)) else {
            return;
        };
        let Some(existing) = item.as_value() else {
            return;
        };
        // Preserve the value's decor (surrounding whitespace) so only the number
        // or string itself changes on disk.
        let decor = existing.decor().clone();
        let mut new: Value = match val {
            EditVal::Int(i) => (*i).into(),
            EditVal::Float(f) => (*f).into(),
            EditVal::Bool(b) => (*b).into(),
            EditVal::Str(s) => s.as_str().into(),
        };
        *new.decor_mut() = decor;
        *item = Item::Value(new);
        self.dirty = true;
    }

    /// The directory backups are written to and read from: `radio-backups` next
    /// to the file.
    fn backup_dir(&self) -> PathBuf {
        let dir = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join("radio-backups")
    }

    /// Copy the current on-disk file into the backup directory under a
    /// timestamped name. `Ok(None)` when there is no existing file to back up.
    fn backup_existing(&self) -> Result<Option<PathBuf>, String> {
        if !self.path.exists() {
            return Ok(None);
        }
        let dir = self.backup_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let stem = self
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("RADIO.toml");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dest = dir.join(format!("{stem}.{stamp}.bak"));
        std::fs::copy(&self.path, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        Ok(Some(dest))
    }

    /// Back up the previous file, then write the current document. Returns the
    /// backup path when one was made.
    pub fn save(&mut self) -> Result<Option<PathBuf>, String> {
        let backup = self.backup_existing()?;
        std::fs::write(&self.path, self.doc.to_string())
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        self.dirty = false;
        Ok(backup)
    }

    /// The kept backups, newest first. Filenames carry a unix-seconds stamp so a
    /// reverse sort orders them by age.
    pub fn backups(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(self.backup_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "bak"))
            .collect();
        v.sort();
        v.reverse();
        v
    }

    /// Load a backup's contents into this document (keeping `path` pointed at
    /// the live file), marking it dirty so a Save writes it back as current.
    pub fn restore(&mut self, backup: &Path) -> Result<(), String> {
        let text =
            std::fs::read_to_string(backup).map_err(|e| format!("{}: {e}", backup.display()))?;
        let doc: DocumentMut = text.parse().map_err(|e| format!("{}: {e}", backup.display()))?;
        self.fields = collect_fields(&doc);
        self.doc = doc;
        self.dirty = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# a comment that must survive a round-trip
[radio]
frequency_hz = 915000000
frequency_hz_description = \"RF center frequency.\"

spreading_factor = 7

[gps]
gps_enabled = true
power_mode = \"full\"
power_mode_type = \"enum:full,psmoo,psmct\"
";

    fn doc() -> RadioDoc {
        RadioDoc {
            doc: SAMPLE.parse().unwrap(),
            fields: collect_fields(&SAMPLE.parse().unwrap()),
            path: PathBuf::from("RADIO.toml"),
            dirty: false,
        }
    }

    #[test]
    fn collects_fields_and_skips_metadata() {
        let d = doc();
        let keys: Vec<_> = d.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["frequency_hz", "spreading_factor", "gps_enabled", "power_mode"]);
        // No `_description` / `_type` key leaks in as an editable field.
        assert!(!keys.iter().any(|k| k.contains("_description") || k.contains("_type")));
    }

    #[test]
    fn infers_and_overrides_types() {
        let d = doc();
        let ty = |k: &str| &d.fields.iter().find(|f| f.key == k).unwrap().ty;
        assert_eq!(ty("frequency_hz"), &FieldType::Int);
        assert_eq!(ty("gps_enabled"), &FieldType::Bool);
        assert_eq!(
            ty("power_mode"),
            &FieldType::Enum(vec!["full".into(), "psmoo".into(), "psmct".into()])
        );
    }

    #[test]
    fn description_is_read() {
        let d = doc();
        let f = d.fields.iter().find(|f| f.key == "frequency_hz").unwrap();
        assert_eq!(f.description.as_deref(), Some("RF center frequency."));
    }

    #[test]
    fn apply_changes_only_the_value_and_keeps_comments() {
        let mut d = doc();
        d.apply("radio", "frequency_hz", &EditVal::Int(868000000));
        d.apply("gps", "power_mode", &EditVal::Str("psmct".into()));
        let out = d.doc.to_string();
        assert!(out.contains("frequency_hz = 868000000"));
        assert!(out.contains("power_mode = \"psmct\""));
        // Untouched context is preserved verbatim.
        assert!(out.contains("# a comment that must survive a round-trip"));
        assert!(out.contains("frequency_hz_description = \"RF center frequency.\""));
        assert!(d.dirty);
    }
}
