//! Every translation key used in the source must exist in the English locale file.

use std::collections::BTreeSet;
use std::path::Path;

use serde_yaml_ng::Value;

/// Flattens `a: { b: { en: "…" } }` into keys like `a.b` that have an `en` translation.
fn collect(prefix: &str, value: &Value, keys: &mut BTreeSet<String>) {
    let Value::Mapping(map) = value else { return };
    for (k, v) in map {
        let Some(k) = k.as_str() else { continue };
        if k == "en" && v.is_string() {
            keys.insert(prefix.to_owned());
        } else if !k.starts_with('_') {
            let path = if prefix.is_empty() {
                k.to_owned()
            } else {
                format!("{prefix}.{k}")
            };
            collect(&path, v, keys);
        }
    }
}

fn source_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            source_files(&path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn every_key_has_an_english_translation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let yaml: Value =
        serde_yaml_ng::from_str(&std::fs::read_to_string(root.join("locales/app.yml")).unwrap())
            .unwrap();
    let mut known = BTreeSet::new();
    collect("", &yaml, &mut known);

    let pattern = regex::Regex::new(r#"\b(?:tr|t)!\(\s*"([a-z0-9_.]+)""#).unwrap();
    let mut files = Vec::new();
    source_files(&root.join("src"), &mut files);
    let mut missing = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for capture in pattern.captures_iter(&text) {
            let key = &capture[1];
            if !known.contains(key) {
                missing.push(format!("{key} ({})", file.display()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "missing translations:\n{}",
        missing.join("\n")
    );
}

#[test]
fn english_copy_follows_the_style_rules() {
    let text =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("locales/app.yml"))
            .unwrap();
    for (n, line) in text.lines().enumerate() {
        assert!(
            !line.contains("..."),
            "line {}: use the ellipsis character (…)",
            n + 1
        );
        if let Some(value) = line.trim().strip_prefix("en:") {
            assert!(
                !value
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "line {}: CJK text in the English locale",
                n + 1
            );
        }
    }
}
