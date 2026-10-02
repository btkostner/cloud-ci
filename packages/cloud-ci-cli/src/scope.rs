//! Resolves each uploaded file's monorepo scope by walking up from the file
//! to the nearest `package.json` and reading its `"name"` field, per
//! `docs/design/byo-ci.md`'s Playwright walkthrough ("The CLI gives each
//! file the scope of its nearest `package.json`").

use std::path::Path;

/// Returns the nearest ancestor `package.json`'s `"name"` field, or an empty
/// scope if no `package.json` is found (or it has no `name`).
pub fn scope_for(path: &Path) -> String {
    let mut dir = path.parent();
    while let Some(d) = dir {
        let manifest = d.join("package.json");
        if let Ok(contents) = std::fs::read_to_string(&manifest) {
            return serde_json::from_str::<serde_json::Value>(&contents)
                .ok()
                .and_then(|json| {
                    json.get("name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
        }
        dir = d.parent();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cloud-ci-cli-scope-test-{label}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn finds_nearest_package_json_name() {
        let tmp = scratch_dir("nearest");
        let pkg_dir = tmp.join("apps/web");
        let results_dir = pkg_dir.join("test-results");
        let _ = fs::create_dir_all(&results_dir);
        let _ = fs::write(pkg_dir.join("package.json"), r#"{"name":"@acme/web"}"#);

        assert_eq!(scope_for(&results_dir.join("e2e.json")), "@acme/web");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn empty_scope_when_no_manifest_found() {
        let tmp = scratch_dir("none");
        let _ = fs::create_dir_all(&tmp);

        assert_eq!(scope_for(&tmp.join("report.json")), "");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn empty_scope_when_manifest_has_no_name() {
        let tmp = scratch_dir("noname");
        let _ = fs::create_dir_all(&tmp);
        let _ = fs::write(tmp.join("package.json"), r#"{"private":true}"#);

        assert_eq!(scope_for(&tmp.join("report.json")), "");

        let _ = fs::remove_dir_all(&tmp);
    }
}
