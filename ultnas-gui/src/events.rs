//! Turning journal entries into desktop notifications.

use std::path::{Path, PathBuf};
use ultnas_core::{journal::CHECKPOINT, JournalEntry, JournalOp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Stripped,
    Restored,
    RestoreFailed,
    Quarantined,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub title: String,
    pub body: String,
}

/// The notification for `entry`, if it's one people should hear about.
/// `home` shortens paths to `~/…`.
pub fn notice(entry: &JournalEntry, home: Option<&Path>) -> Option<Notice> {
    // A rotation re-writes past escalations at the top of the new file.
    if entry.detail.as_deref() == Some(CHECKPOINT) {
        return None;
    }
    let path = entry.path.as_deref().map(|p| display_path(p, home));
    let (kind, title, body) = match entry.op {
        JournalOp::IntegritySanitize => {
            let n = entry.size.unwrap_or(0);
            let what = if n == 1 {
                "1 invisible character".to_string()
            } else {
                format!("{n} invisible characters")
            };
            (
                NoticeKind::Stripped,
                "Invisible characters removed",
                format!("Stripped {what} from {}.", path.as_deref()?),
            )
        }
        JournalOp::IntegrityRestore => (
            NoticeKind::Restored,
            "File restored",
            match &path {
                Some(p) => {
                    format!("{p} was recreated from its stable copy after repeated tampering.")
                }
                None => "A sealed record was restored from a verified copy.".to_string(),
            },
        ),
        JournalOp::IntegrityRestoreFailed => {
            let why = entry.detail.as_deref().unwrap_or("no detail");
            let why = why.strip_prefix("action=sanitize ").unwrap_or(why);
            (
                NoticeKind::RestoreFailed,
                "Ultnas couldn't repair a file",
                match &path {
                    Some(p) => format!("{p}: {why}"),
                    None => why.to_string(),
                },
            )
        }
        JournalOp::IntegrityEscalate => {
            let ns = match entry.ns.as_deref() {
                None | Some("") => "(root)",
                Some(ns) => ns,
            };
            (
                NoticeKind::Quarantined,
                "Namespace quarantined",
                format!(
                    "Ultnas stopped repairing “{ns}” after repeated restores. \
                     Review it, then lift the quarantine."
                ),
            )
        }
        JournalOp::VersionPending => (
            NoticeKind::Pending,
            "Edit waiting for approval",
            format!(
                "{} changed. Approve the edit to make it the stable version.",
                path.as_deref()?
            ),
        ),
        _ => return None,
    };
    Some(Notice {
        kind,
        title: title.to_string(),
        body,
    })
}

/// `path` with the home directory shown as `~`.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(rest) = home.and_then(|h| path.strip_prefix(h).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return PathBuf::from("~").join(rest).display().to_string();
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn entry(op: JournalOp, path: Option<&str>, size: Option<u64>, detail: &str) -> JournalEntry {
        JournalEntry {
            ts: Utc::now(),
            op,
            id: None,
            ns: Some("etc".into()),
            label: None,
            size,
            detail: Some(detail.into()),
            path: path.map(PathBuf::from),
        }
    }

    #[test]
    fn stripping_names_the_count_and_the_file() {
        let home = Path::new("/home/ada");
        let n = notice(
            &entry(
                JournalOp::IntegritySanitize,
                Some("/home/ada/.bashrc"),
                Some(2),
                "removed=2",
            ),
            Some(home),
        )
        .unwrap();
        assert_eq!(n.kind, NoticeKind::Stripped);
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            n.body,
            format!("Stripped 2 invisible characters from ~{sep}.bashrc.")
        );

        let one = notice(
            &entry(JournalOp::IntegritySanitize, Some("/x"), Some(1), ""),
            None,
        )
        .unwrap();
        assert!(
            one.body.contains("1 invisible character from"),
            "{}",
            one.body
        );
    }

    #[test]
    fn each_alerting_op_has_a_notice() {
        for (op, kind) in [
            (JournalOp::IntegrityRestore, NoticeKind::Restored),
            (JournalOp::IntegrityRestoreFailed, NoticeKind::RestoreFailed),
            (JournalOp::IntegrityEscalate, NoticeKind::Quarantined),
            (JournalOp::VersionPending, NoticeKind::Pending),
        ] {
            let n = notice(&entry(op, Some("/f"), None, "x"), None).unwrap();
            assert_eq!(n.kind, kind);
        }
    }

    #[test]
    fn routine_entries_and_checkpoints_stay_quiet() {
        assert!(notice(&entry(JournalOp::TrackFile, Some("/f"), None, "x"), None).is_none());
        assert!(notice(
            &entry(JournalOp::IntegrityEscalate, None, None, CHECKPOINT),
            None
        )
        .is_none());
    }

    #[test]
    fn failed_sanitize_drops_the_action_prefix() {
        let n = notice(
            &entry(
                JournalOp::IntegrityRestoreFailed,
                Some("/f"),
                None,
                "action=sanitize permission denied",
            ),
            None,
        )
        .unwrap();
        assert!(n.body.ends_with(": permission denied"), "{}", n.body);
    }

    #[test]
    fn paths_outside_home_are_shown_whole() {
        assert_eq!(
            display_path(Path::new("/etc/hosts"), Some(Path::new("/home/ada"))),
            Path::new("/etc/hosts").display().to_string()
        );
        assert_eq!(
            display_path(Path::new("/home/ada"), Some(Path::new("/home/ada"))),
            "~"
        );
    }
}
