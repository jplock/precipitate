//! Workspaces. A workspace is one estate under assessment: a SQLite file
//! holding its scans and plan. It is not an Oxide project; those are the
//! target tenancy each workload and environment pair maps to.

use camino::Utf8PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: Utf8PathBuf,
    pub modified: Option<SystemTime>,
}

/// The directory workspaces live in, one `<name>.db` each.
#[derive(Debug, Clone)]
pub struct WorkspaceDir(pub Utf8PathBuf);

impl WorkspaceDir {
    pub fn path_for(&self, name: &str) -> Utf8PathBuf {
        self.0.join(format!("{name}.db"))
    }

    /// Existing workspaces, most recently modified first. A missing
    /// directory is an empty list, not an error.
    pub fn list(&self) -> std::io::Result<Vec<WorkspaceEntry>> {
        let mut entries = Vec::new();
        let dir = match std::fs::read_dir(&self.0) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(entries);
            }
            Err(e) => return Err(e),
        };
        for entry in dir {
            let entry = entry?;
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                continue;
            };
            if path.extension() != Some("db") {
                continue;
            }
            let Some(name) = path.file_stem() else { continue };
            let modified = entry.metadata().and_then(|m| m.modified()).ok();
            entries.push(WorkspaceEntry {
                name: name.to_string(),
                path,
                modified,
            });
        }
        entries.sort_by(|a, b| {
            b.modified.cmp(&a.modified).then(a.name.cmp(&b.name))
        });
        Ok(entries)
    }

    pub(crate) fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.0)
    }
}

/// Names become file stems, so keep them to a safe character set.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_restricted_to_file_safe_characters() {
        assert!(valid_name("acme-prod_2026"));
        assert!(!valid_name(""));
        assert!(!valid_name("has space"));
        assert!(!valid_name("../escape"));
    }

    #[test]
    fn missing_directory_lists_nothing() {
        let dir = WorkspaceDir(Utf8PathBuf::from("/nonexistent/precip-test"));
        assert!(dir.list().unwrap().is_empty());
    }
}
