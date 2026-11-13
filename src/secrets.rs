//! Secrets and identities from the environment: directly (`VAR`) or from a
//! file named by `VAR_FILE` (Docker / Compose secrets mounted under
//! `/run/secrets/...`).
//!
//! One helper, [`env_or_file`], serves every such variable
//! (`HL_PRIVATE_KEY`, `HL_ACCOUNT_ADDRESS`, `HL_VAULT_ADDRESS`,
//! `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`):
//!
//! * `VAR` alone: used as before (trimmed; empty counts as unset);
//! * `VAR_FILE` alone: the file is read as UTF-8 and trimmed (a trailing
//!   newline from `echo` / an editor is fine). A missing, unreadable,
//!   non-UTF-8 or empty file is an error naming the variable and the path;
//! * both non-empty: an error (never silently prefer one of them);
//! * neither: `Ok(None)`.
//!
//! Errors never contain the file content. The value and every temporary
//! buffer holding it are [`Zeroizing`], wiped on drop.

use zeroize::{Zeroize, Zeroizing};

/// Suffix of the "read the value from this file" variant of a variable.
pub const FILE_SUFFIX: &str = "_FILE";

/// Name of the `*_FILE` variant of `var`.
pub fn file_var(var: &str) -> String {
    format!("{var}{FILE_SUFFIX}")
}

/// Why a variable could not be resolved. Only names and paths, never the
/// value or file bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("both {var} and {var}_FILE are set: set only one of them")]
    Conflict { var: String },
    #[error("{var}_FILE={path}: cannot read the file ({reason})")]
    Unreadable {
        var: String,
        path: String,
        reason: String,
    },
    #[error("{var}_FILE={path}: the file is empty")]
    Empty { var: String, path: String },
}

fn non_empty(v: Option<String>) -> Option<Zeroizing<String>> {
    let v = Zeroizing::new(v?);
    let t = v.trim();
    (!t.is_empty()).then(|| Zeroizing::new(t.to_string()))
}

/// Read a secret file: UTF-8, trimmed, non-empty.
pub fn read_secret_file(var: &str, path: &str) -> Result<Zeroizing<String>, SecretError> {
    let unreadable = |reason: String| SecretError::Unreadable {
        var: var.to_string(),
        path: path.to_string(),
        reason,
    };
    let bytes = std::fs::read(path).map_err(|e| unreadable(io_reason(&e)))?;
    let text = match String::from_utf8(bytes) {
        Ok(s) => Zeroizing::new(s),
        Err(e) => {
            let mut b = e.into_bytes();
            b.zeroize();
            return Err(unreadable("not valid UTF-8".into()));
        }
    };
    let t = text.trim();
    if t.is_empty() {
        return Err(SecretError::Empty {
            var: var.to_string(),
            path: path.to_string(),
        });
    }
    Ok(Zeroizing::new(t.to_string()))
}

/// An I/O error as text without anything read from the file.
fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "no such file".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        _ => e.to_string(),
    }
}

/// Resolve `var` from the environment or from the file named by
/// `var_FILE` (see the module docs).
pub fn env_or_file(
    env: &impl Fn(&str) -> Option<String>,
    var: &str,
) -> Result<Option<Zeroizing<String>>, SecretError> {
    let direct = non_empty(env(var));
    let file = non_empty(env(&file_var(var)));
    match (direct, file) {
        (Some(_), Some(_)) => Err(SecretError::Conflict {
            var: var.to_string(),
        }),
        (Some(v), None) => Ok(Some(v)),
        (None, Some(path)) => read_secret_file(var, &path).map(Some),
        (None, None) => Ok(None),
    }
}

/// Whether `var` or `var_FILE` is set to something non-empty (no file
/// access). Used for status lines.
pub fn is_configured(env: &impl Fn(&str) -> Option<String>, var: &str) -> bool {
    let set = |k: &str| env(k).is_some_and(|v| !v.trim().is_empty());
    set(var) || set(&file_var(var))
}

/// Where a configured value comes from, for status lines (`VAR` or
/// `VAR_FILE=<path>`). Never the value.
pub fn source_label(env: &impl Fn(&str) -> Option<String>, var: &str) -> Option<String> {
    let fv = file_var(var);
    match (non_empty(env(var)), non_empty(env(&fv))) {
        (Some(_), _) => Some(var.to_string()),
        (None, Some(p)) => Some(format!("{fv}={}", p.as_str())),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn direct_value_is_trimmed_and_empty_is_unset() {
        let env = env_of(&[("X", "  abc \n")]);
        assert_eq!(env_or_file(&env, "X").unwrap().unwrap().as_str(), "abc");
        let env = env_of(&[("X", "   "), ("X_FILE", "")]);
        assert_eq!(env_or_file(&env, "X").unwrap(), None);
        assert_eq!(env_or_file(&env_of(&[]), "X").unwrap(), None);
    }

    #[test]
    fn file_value_is_read_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret");
        std::fs::write(&p, "\n  s3cr3t-value \r\n\n").unwrap();
        let path = p.to_str().unwrap();
        let env = env_of(&[("X_FILE", path)]);
        assert_eq!(
            env_or_file(&env, "X").unwrap().unwrap().as_str(),
            "s3cr3t-value"
        );
        assert!(is_configured(&env, "X"));
        assert_eq!(source_label(&env, "X").unwrap(), format!("X_FILE={path}"));
    }

    #[test]
    fn both_set_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret");
        std::fs::write(&p, "from-file").unwrap();
        let env = env_of(&[("X", "from-env"), ("X_FILE", p.to_str().unwrap())]);
        let e = env_or_file(&env, "X").unwrap_err();
        assert_eq!(e, SecretError::Conflict { var: "X".into() });
        let msg = e.to_string();
        assert!(msg.contains("X and X_FILE"), "{msg}");
        assert!(!msg.contains("from-env") && !msg.contains("from-file"));
    }

    #[test]
    fn missing_empty_and_binary_files_are_explicit_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let env = env_of(&[("X_FILE", missing.to_str().unwrap())]);
        let e = env_or_file(&env, "X").unwrap_err();
        assert!(matches!(&e, SecretError::Unreadable { var, .. } if var == "X"));
        let msg = e.to_string();
        assert!(msg.contains("X_FILE") && msg.contains("nope"), "{msg}");

        let empty = dir.path().join("empty");
        std::fs::write(&empty, " \n\t\n").unwrap();
        let env = env_of(&[("X_FILE", empty.to_str().unwrap())]);
        assert!(matches!(
            env_or_file(&env, "X").unwrap_err(),
            SecretError::Empty { .. }
        ));

        let bin = dir.path().join("bin");
        std::fs::write(&bin, [0xff, 0xfe, b'k', b'e', b'y']).unwrap();
        let env = env_of(&[("X_FILE", bin.to_str().unwrap())]);
        let e = env_or_file(&env, "X").unwrap_err();
        assert!(e.to_string().contains("UTF-8"), "{e}");

        // A directory is unreadable as a file.
        let env = env_of(&[("X_FILE", dir.path().to_str().unwrap())]);
        assert!(matches!(
            env_or_file(&env, "X").unwrap_err(),
            SecretError::Unreadable { .. }
        ));
    }

    #[test]
    fn errors_never_contain_file_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret");
        std::fs::write(&p, "TOPSECRET").unwrap();
        let env = env_of(&[("X", "OTHERSECRET"), ("X_FILE", p.to_str().unwrap())]);
        let e = env_or_file(&env, "X").unwrap_err();
        for s in [e.to_string(), format!("{e:?}")] {
            assert!(
                !s.contains("TOPSECRET") && !s.contains("OTHERSECRET"),
                "{s}"
            );
        }
        assert_eq!(source_label(&env, "X").unwrap(), "X");
    }
}
