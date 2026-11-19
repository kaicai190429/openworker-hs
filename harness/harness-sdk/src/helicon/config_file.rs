//! Where the memory extension's configuration comes from, and whether it is on.
//!
//! Nothing changes for an application with no such file: the extension is only attached when a
//! configuration is found and enabled. Lookup order:
//!   1. the file named by `OPENWORKER_HELICON_CONFIG`
//!   2. `<state_dir>/helicon-memory.json`
//!   3. `helicon-memory.json` next to the executable, or in its `resources/` directory (a packaged
//!      application's own resources, so its users configure nothing)
//!
//! The file holds the options of [`MemoryConfig`] plus `"enabled"` (default true). The credential is
//! not written into it: `"apiKey": "${HELICON_TENANT_TOKEN}"` is read from the process environment.
use super::config::{MemoryConfig, UNCONFIGURED_API_KEY};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(crate) const CONFIG_ENV: &str = "OPENWORKER_HELICON_CONFIG";
pub(crate) const CONFIG_FILE: &str = "helicon-memory.json";

#[derive(Debug)]
pub(crate) enum ConfigLoad {
    Absent,
    Disabled,
    Invalid { path: PathBuf, reason: String },
    Enabled(Box<MemoryConfig>),
}

/// The directories of a packaged application whose own resources may hold the file.
pub(crate) fn resource_dirs() -> Vec<PathBuf> {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return vec![];
    };
    vec![dir.clone(), dir.join("resources"), dir.join("../Resources")]
}

pub(crate) fn load(
    state_dir: &Path,
    resources: &[PathBuf],
    env: &dyn Fn(&str) -> Option<String>,
) -> ConfigLoad {
    let from_env = env(CONFIG_ENV)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let mut candidates = vec![state_dir.join(CONFIG_FILE)];
    candidates.extend(resources.iter().map(|d| d.join(CONFIG_FILE)));
    let path = match &from_env {
        Some(p) => PathBuf::from(p),
        None => candidates
            .iter()
            .find(|p| p.is_file())
            .cloned()
            .unwrap_or_else(|| candidates[0].clone()),
    };
    if !path.is_file() {
        // an explicit path that is missing is a mistake worth reporting; the default one is just "off"
        return if from_env.is_some() {
            ConfigLoad::Invalid {
                path,
                reason: "file not found".into(),
            }
        } else {
            ConfigLoad::Absent
        };
    }
    let invalid = |path: &Path, reason: String| ConfigLoad::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return invalid(&path, format!("could not be read: {e}")),
    };
    let raw: Value = match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
        Ok(v) => v,
        Err(e) => return invalid(&path, format!("not valid JSON: {e}")),
    };
    let Value::Object(mut map) = raw else {
        return invalid(&path, "expected a JSON object".into());
    };
    match map.remove("enabled") {
        Some(Value::Bool(false)) => return ConfigLoad::Disabled,
        None | Some(Value::Bool(true)) => {}
        Some(_) => return invalid(&path, "enabled must be true or false".into()),
    }
    match MemoryConfig::parse(&Value::Object(map), env) {
        Ok(cfg) if &*cfg.api_key == UNCONFIGURED_API_KEY => invalid(
            &path,
            "apiKey is missing, or the environment variable it names is not set".into(),
        ),
        Ok(cfg) => ConfigLoad::Enabled(Box::new(cfg)),
        Err(reason) => invalid(&path, reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(vars: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        }
    }

    fn write(dir: &Path, text: &str) {
        std::fs::write(dir.join(CONFIG_FILE), text).unwrap();
    }

    #[test]
    fn no_file_means_off() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load(dir.path(), &[], &env_with(vec![])),
            ConfigLoad::Absent
        ));
    }

    #[test]
    fn enabled_false_is_off_and_a_good_file_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with(vec![("TOKEN", "hs_x".into())]);
        write(dir.path(), r#"{"enabled":false,"baseUrl":"http://h"}"#);
        assert!(matches!(load(dir.path(), &[], &env), ConfigLoad::Disabled));
        write(
            dir.path(),
            r#"{"baseUrl":"http://h:5001","apiKey":"${TOKEN}"}"#,
        );
        match load(dir.path(), &[], &env) {
            ConfigLoad::Enabled(cfg) => assert_eq!(&*cfg.api_key, "hs_x"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_missing_credential_is_invalid_and_so_is_bad_json() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"{"baseUrl":"http://h","apiKey":"${NOPE}"}"#);
        assert!(matches!(
            load(dir.path(), &[], &env_with(vec![])),
            ConfigLoad::Invalid { .. }
        ));
        write(dir.path(), "{not json");
        assert!(matches!(
            load(dir.path(), &[], &env_with(vec![])),
            ConfigLoad::Invalid { .. }
        ));
    }

    #[test]
    fn an_explicit_path_wins_and_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"{"baseUrl":"http://h","apiKey":"k"}"#);
        let missing = env_with(vec![(
            CONFIG_ENV,
            dir.path().join("none.json").display().to_string(),
        )]);
        assert!(matches!(
            load(dir.path(), &[], &missing),
            ConfigLoad::Invalid { .. }
        ));
    }

    #[test]
    fn a_packaged_resources_directory_is_the_last_place_to_look() {
        let state = tempfile::tempdir().unwrap();
        let res = tempfile::tempdir().unwrap();
        write(res.path(), r#"{"baseUrl":"http://h","apiKey":"k"}"#);
        assert!(matches!(
            load(state.path(), &[res.path().to_path_buf()], &env_with(vec![])),
            ConfigLoad::Enabled(_)
        ));
    }
}
