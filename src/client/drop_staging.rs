//! fork: drop staging
//!
//! macOS keeps the live screenshot and screen recording thumbnail in
//! `$TMPDIR/TemporaryItems/NSIRD_*/` until it is saved. A terminal turns a drop
//! of that thumbnail into a pasted path, but only the terminal app and the
//! processes it is responsible for may read the file. The client is one of them.
//! The server runs as its own launchd job, so pane processes are denied the read
//! and an agent silently discards the drop (herdrdev/herdr#2091).
//!
//! For a local session the client therefore copies such a file into a plain
//! temporary directory and forwards that path instead, keeping the paste framing.
//! Every other input passes through untouched.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
const PROTECTED_MARKER: &str = "/TemporaryItems/NSIRD_";
const STAGING_DIR_NAME: &str = "herdr-drops";
/// A dropped path is short. Larger input is ordinary pasted text and is not inspected.
const MAX_DROP_TEXT_BYTES: usize = 8 * 1024;
const STAGED_FILE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Returns the input to forward in place of `data` when it is a drop of a
/// protected macOS temporary item. `None` means forward `data` unchanged.
pub(super) fn stage_protected_drop(data: &[u8]) -> Option<Vec<u8>> {
    let source = protected_drop_path(data)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let staging_dir = std::env::temp_dir().join(STAGING_DIR_NAME);
    match stage_file(&source, &staging_dir, now) {
        Ok(staged) => {
            info!(staged = %staged.display(), "staged protected temporary drop for the pane");
            Some(drop_input(data, &staged))
        }
        Err(err) => {
            warn!(
                source = %source.display(),
                err = %err,
                "failed to stage protected temporary drop; forwarding the original path"
            );
            None
        }
    }
}

/// The dropped path when `data` is a single path below a protected temporary
/// item directory, bracketed or not.
fn protected_drop_path(data: &[u8]) -> Option<PathBuf> {
    let payload = paste_payload(data).unwrap_or(data);
    if payload.len() > MAX_DROP_TEXT_BYTES {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?;
    let text = text.trim_end_matches(['\r', '\n', ' ']);
    if text.is_empty() || text.contains(['\r', '\n', '\x1b']) {
        return None;
    }
    let path = PathBuf::from(unquote_drop_path(text));
    (path.is_absolute() && path.to_str()?.contains(PROTECTED_MARKER)).then_some(path)
}

fn paste_payload(data: &[u8]) -> Option<&[u8]> {
    data.strip_prefix(PASTE_START)?.strip_suffix(PASTE_END)
}

/// Terminals quote a dropped path for a shell: backslash escapes, or one pair
/// of quotes around the whole path.
fn unquote_drop_path(text: &str) -> String {
    for quote in ['\'', '"'] {
        if let Some(inner) = text
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            if !inner.contains(quote) {
                return inner.to_owned();
            }
        }
    }
    let mut path = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => path.push(chars.next().unwrap_or(ch)),
            _ => path.push(ch),
        }
    }
    path
}

fn stage_file(source: &Path, staging_dir: &Path, now: Duration) -> std::io::Result<PathBuf> {
    if !std::fs::metadata(source)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "dropped path is not a regular file",
        ));
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(staging_dir)?;
    prune_staged_files(staging_dir, now);
    let staged = staging_dir.join(staged_file_name(source, now));
    // On APFS this is a clone, so a large recording costs neither time nor space.
    std::fs::copy(source, &staged)?;
    Ok(staged)
}

/// `<unix millis>-<name>`, with the name reduced to characters that need no
/// quoting in any shell or agent prompt.
fn staged_file_name(source: &Path, now: Duration) -> String {
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("drop");
    let mut safe = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            safe.push(ch);
        } else if !safe.ends_with('-') {
            safe.push('-');
        }
    }
    format!(
        "{}-{}",
        now.as_millis(),
        safe.trim_start_matches(['-', '.'])
    )
}

/// Removes staged files older than a day. The age comes from the file name,
/// because a cloned file keeps the modification time of its source.
fn prune_staged_files(staging_dir: &Path, now: Duration) {
    let Ok(entries) = std::fs::read_dir(staging_dir) else {
        return;
    };
    let cutoff = now.saturating_sub(STAGED_FILE_MAX_AGE).as_millis();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let staged_at = name
            .to_str()
            .and_then(|name| name.split_once('-'))
            .and_then(|(millis, _)| millis.parse::<u128>().ok());
        if staged_at.is_some_and(|staged_at| staged_at < cutoff) {
            if let Err(err) = std::fs::remove_file(entry.path()) {
                warn!(path = %entry.path().display(), err = %err, "failed to remove old staged drop");
            }
        }
    }
}

/// The staged path as terminal input, framed the way the original drop was.
fn drop_input(original: &[u8], staged: &Path) -> Vec<u8> {
    let path = escape_drop_path(&staged.to_string_lossy());
    if paste_payload(original).is_some() {
        [PASTE_START, path.as_bytes(), PASTE_END].concat()
    } else {
        path.into_bytes()
    }
}

/// Backslash-escapes a path the way a terminal does for a dropped file.
fn escape_drop_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for ch in path.chars() {
        if ch.is_whitespace() || "\\'\"()[]{}&;|<>*?$`!#~".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "herdr-drop-staging-test-{label}-{}-{:x}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn thumbnail(&self, name: &str, contents: &[u8]) -> PathBuf {
            let dir = self.0.join("TemporaryItems/NSIRD_screencaptureui_abc123");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }

        fn staging_dir(&self) -> PathBuf {
            self.0.join(STAGING_DIR_NAME)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn bracketed(text: &str) -> Vec<u8> {
        [PASTE_START, text.as_bytes(), PASTE_END].concat()
    }

    const THUMBNAIL: &str =
        "/var/folders/v5/x/T/TemporaryItems/NSIRD_screencaptureui_nuOyKN/Screenshot 2026-11-18 at 10.43.16.png";

    #[test]
    fn recognizes_backslash_escaped_thumbnail_drop() {
        let drop = bracketed(&THUMBNAIL.replace(' ', "\\ "));
        assert_eq!(protected_drop_path(&drop), Some(PathBuf::from(THUMBNAIL)));
    }

    #[test]
    fn recognizes_quoted_and_unbracketed_thumbnail_drops() {
        for text in [
            format!("'{THUMBNAIL}'"),
            format!("\"{THUMBNAIL}\""),
            format!("{} ", THUMBNAIL.replace(' ', "\\ ")),
        ] {
            assert_eq!(
                protected_drop_path(&bracketed(&text)),
                Some(PathBuf::from(THUMBNAIL)),
                "{text:?}"
            );
            assert_eq!(
                protected_drop_path(text.as_bytes()),
                Some(PathBuf::from(THUMBNAIL)),
                "{text:?}"
            );
        }
    }

    #[test]
    fn leaves_other_input_alone() {
        let long_paste = format!("{THUMBNAIL}{}", "x".repeat(MAX_DROP_TEXT_BYTES));
        for data in [
            b"a".to_vec(),
            b"\x1b[A".to_vec(),
            bracketed(""),
            bracketed("/Users/me/Desktop/Screenshot\\ 1.png"),
            bracketed("TemporaryItems/NSIRD_x/Screenshot.png"),
            bracketed(&format!("{THUMBNAIL}\nsecond line")),
            bracketed(&format!("see {THUMBNAIL}")),
            bracketed(&long_paste),
            [b"\x1b[O".as_slice(), &bracketed(THUMBNAIL)].concat(),
        ] {
            assert_eq!(protected_drop_path(&data), None, "{data:?}");
        }
    }

    #[test]
    fn stages_a_readable_copy_and_keeps_the_paste_framing() {
        let scratch = Scratch::new("copy");
        let source = scratch.thumbnail("Screenshot 2026-11-18 at 10.43.16.png", b"png bytes");
        let now = Duration::from_millis(1_795_000_000_000);

        let staged = stage_file(&source, &scratch.staging_dir(), now).unwrap();

        assert_eq!(
            staged,
            scratch
                .staging_dir()
                .join("1795000000000-Screenshot-2026-11-18-at-10.43.16.png")
        );
        assert_eq!(std::fs::read(&staged).unwrap(), b"png bytes");
        assert!(!staged.to_string_lossy().contains(PROTECTED_MARKER));

        let escaped = escape_drop_path(&staged.to_string_lossy());
        let original = bracketed(&source.to_string_lossy().replace(' ', "\\ "));
        assert_eq!(drop_input(&original, &staged), bracketed(&escaped));
        assert_eq!(
            drop_input(source.to_string_lossy().as_bytes(), &staged),
            escaped.into_bytes()
        );
    }

    #[test]
    fn staged_names_need_no_quoting() {
        let now = Duration::from_millis(7);
        for (name, expected) in [
            (
                "Screen Recording 2026-11-17 at 22.07.01.mov",
                "7-Screen-Recording-2026-11-17-at-22.07.01.mov",
            ),
            (
                "Skärmavbild 2026-11-18 kl. 10.43.16.png",
                "7-Sk-rmavbild-2026-11-18-kl.-10.43.16.png",
            ),
            (
                "Screenshot 10.43.16\u{202f}AM (2).png",
                "7-Screenshot-10.43.16-AM-2-.png",
            ),
            (".hidden", "7-hidden"),
        ] {
            let staged = staged_file_name(Path::new("/tmp").join(name).as_path(), now);
            assert_eq!(staged, expected);
            assert_eq!(escape_drop_path(&staged), staged);
        }
    }

    #[test]
    fn escapes_a_staging_directory_that_needs_it() {
        assert_eq!(
            escape_drop_path("/tmp/my dir (1)/herdr-drops/7-a.png"),
            "/tmp/my\\ dir\\ \\(1\\)/herdr-drops/7-a.png"
        );
    }

    #[test]
    fn missing_source_is_an_error_so_the_original_path_is_forwarded() {
        let scratch = Scratch::new("missing");
        let source = scratch
            .0
            .join("TemporaryItems/NSIRD_screencaptureui_gone/Screenshot.png");

        assert!(stage_file(&source, &scratch.staging_dir(), Duration::from_millis(1)).is_err());
        assert!(!scratch.staging_dir().exists());
    }

    #[test]
    fn directory_drop_is_not_staged() {
        let scratch = Scratch::new("dir");
        let source = scratch.thumbnail("Screenshot.png", b"x");
        let directory = source.parent().unwrap();

        assert!(stage_file(directory, &scratch.staging_dir(), Duration::from_millis(1)).is_err());
    }

    #[test]
    fn prunes_staged_files_older_than_a_day() {
        let scratch = Scratch::new("prune");
        let source = scratch.thumbnail("Screenshot.png", b"new");
        let staging_dir = scratch.staging_dir();
        std::fs::create_dir_all(&staging_dir).unwrap();
        let now = Duration::from_millis(1_795_000_000_000);
        let old = now - STAGED_FILE_MAX_AGE - Duration::from_millis(1);
        let recent = now - Duration::from_secs(60);
        let old_file = staging_dir.join(format!("{}-old.png", old.as_millis()));
        let recent_file = staging_dir.join(format!("{}-recent.png", recent.as_millis()));
        let foreign_file = staging_dir.join("notes.txt");
        for file in [&old_file, &recent_file, &foreign_file] {
            std::fs::write(file, b"x").unwrap();
        }

        let staged = stage_file(&source, &staging_dir, now).unwrap();

        assert!(!old_file.exists());
        assert!(recent_file.exists());
        assert!(foreign_file.exists());
        assert!(staged.exists());
    }
}
