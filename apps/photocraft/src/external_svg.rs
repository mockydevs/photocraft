//! Explicit desktop SVG editing. Clipboard/source data never supplies a command or program path.
//!
//! The owned temporary SVG is watched off the UI thread. Closing the PhotoCraft session stops
//! watching and removes its temporary directory; it does not terminate the external editor.

use photocraft_ui_egui::{ExternalSvgEditor, ExternalSvgSession};

/// Start an explicit native editor session; artwork cannot supply executable paths.
pub(crate) fn open(editor: ExternalSvgEditor, source: photocraft_ui_egui::ExternalVectorSource) -> Result<Box<dyn ExternalSvgSession>, String> {
    native::open(editor, source)
}

mod native {
    use super::*;
    use std::fs::{Metadata, OpenOptions};
    use std::io::{Read, Write};
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Child;
    use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime};
    use tempfile::TempDir;

    const MAX_BYTES: usize = 16 * 1024 * 1024;
    const POLL_INTERVAL: Duration = Duration::from_millis(200);
    const SAVE_SETTLE: Duration = Duration::from_millis(600);

    /// Resolve the chosen editor and move conversion, launch and watching off the UI thread.
    pub(super) fn open(editor: ExternalSvgEditor, source: photocraft_ui_egui::ExternalVectorSource) -> Result<Box<dyn ExternalSvgSession>, String> {
        let executable = resolve_editor(editor)?;
        if source.bytes.is_empty() || source.bytes.len() > MAX_BYTES || !matches!(source.extension.as_str(), "svg" | "pdf" | "ai" | "eps") {
            return Err("Unsupported embedded vector source or source exceeds 16 MiB".into());
        }
        // Conversion and editor startup run off the UI thread. Dropping this session cancels
        // conversion; it never kills the interactive editor or changes the parent document.
        let (stop, stopped) = mpsc::channel();
        let (send, updates) = mpsc::sync_channel(2);
        thread::Builder::new()
            .name("photocraft-vector-editor".into())
            .spawn(move || {
                let prepared = super::vector_conversion::prepare(&executable, &source, &stopped);
                match prepared {
                    Ok((owned, initial)) => {
                        if stopped.try_recv().is_ok() {
                            return;
                        }
                        match std::process::Command::new(&executable).arg(&owned.file).spawn() {
                            Ok(child) => watch(owned, initial, Some(child), stopped, send, POLL_INTERVAL, SAVE_SETTLE),
                            Err(error) => {
                                let _ = send.try_send(Err(format!("Could not open the vector editor: {error}")));
                            }
                        }
                    }
                    Err(error) => {
                        let _ = send.try_send(Err(error));
                    }
                }
            })
            .map_err(|e| format!("Cannot start vector editor session: {e}"))?;
        Ok(Box::new(Session { stop, updates }))
    }

    /// Discover only known installations or an explicitly chosen executable.
    #[cfg(unix)]
    fn resolve_editor(editor: ExternalSvgEditor) -> Result<PathBuf, String> {
        match editor {
            ExternalSvgEditor::Program(path) => validate_executable(Path::new(&path)),
            ExternalSvgEditor::VectorCraft => {
                let mut candidates = vec![PathBuf::from("/usr/local/bin/vectorcraft"), PathBuf::from("/usr/bin/vectorcraft")];
                if let Some(home) = std::env::var_os("HOME") {
                    let home = PathBuf::from(home);
                    candidates.insert(0, home.join(".local/share/flatpak/app/ai.storyteller.vectorcraft/current/active/files/bin/vectorcraft"));
                    candidates.push(home.join(".local/bin/vectorcraft"));
                }
                candidates
                    .into_iter()
                    .find_map(|path| validate_executable(&path).ok())
                    .ok_or_else(|| "VectorCraft was not found. Choose its executable with Other application".into())
            }
            ExternalSvgEditor::Illustrator => Err("Choose a local executable with Other application".into()),
        }
    }

    /// Canonicalize an absolute user-selected path and require an executable regular file.
    #[cfg(unix)]
    fn validate_executable(path: &Path) -> Result<PathBuf, String> {
        use std::os::unix::fs::PermissionsExt;
        if !path.is_absolute() {
            return Err("Choose an absolute executable path".into());
        }
        let path = path.canonicalize().map_err(|e| format!("Cannot resolve the chosen editor: {e}"))?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| format!("Cannot read the chosen editor: {e}"))?;
        ensure_regular(&metadata)?;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("The chosen file is not executable".into());
        }
        Ok(path)
    }

    #[cfg(windows)]
    fn resolve_editor(editor: ExternalSvgEditor) -> Result<PathBuf, String> {
        match editor {
            ExternalSvgEditor::Program(path) => validate_executable(Path::new(&path)),
            ExternalSvgEditor::VectorCraft => {
                let mut candidates = vec![];
                for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
                    if let Some(root) = std::env::var_os(variable) {
                        candidates.push(PathBuf::from(root).join("VectorCraft/vectorcraft.exe"));
                    }
                }
                if let Some(root) = std::env::var_os("LOCALAPPDATA") {
                    candidates.push(PathBuf::from(root).join("Programs/VectorCraft/vectorcraft.exe"));
                }
                candidates
                    .into_iter()
                    .find_map(|path| validate_executable(&path).ok())
                    .ok_or_else(|| "VectorCraft was not found. Choose its executable with Other editor".into())
            }
            ExternalSvgEditor::Illustrator => {
                let mut candidates = vec![];
                for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
                    let Some(root) = std::env::var_os(variable) else { continue };
                    let adobe = PathBuf::from(root).join("Adobe");
                    // One known installation level only; never search the user's disks recursively.
                    let Ok(entries) = std::fs::read_dir(&adobe) else { continue };
                    for entry in entries.take(64).flatten() {
                        if !entry.file_name().to_string_lossy().starts_with("Adobe Illustrator") {
                            continue;
                        }
                        candidates.push(entry.path().join("Support Files/Contents/Windows/Illustrator.exe"));
                    }
                }
                candidates.sort();
                candidates
                    .into_iter()
                    .rev()
                    .find_map(|path| validate_executable(&path).ok())
                    .ok_or_else(|| "Illustrator was not found. Choose its executable with Other editor".into())
            }
        }
    }

    #[cfg(windows)]
    fn validate_executable(path: &Path) -> Result<PathBuf, String> {
        if !path.is_absolute() || !path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("exe")) {
            return Err("Choose an absolute Windows .exe path; scripts and command lines are not supported".into());
        }
        let metadata = std::fs::symlink_metadata(path).map_err(|e| format!("Cannot read the chosen editor: {e}"))?;
        ensure_regular(&metadata)?;
        path.canonicalize().map_err(|e| format!("Cannot resolve the chosen editor: {e}"))
    }

    pub(super) struct OwnedSvg {
        directory: TempDir,
        pub(super) root: PathBuf,
        pub(super) file: PathBuf,
    }

    impl OwnedSvg {
        /// Create a private working file from bounded source bytes.
        pub(super) fn create(svg: &[u8]) -> Result<Self, String> {
            if svg.is_empty() || svg.len() > MAX_BYTES {
                return Err("The SVG must contain between 1 byte and 16 MiB".into());
            }
            let directory =
                tempfile::Builder::new().prefix("photocraft-svg-edit-").tempdir().map_err(|e| format!("Cannot create the SVG editing directory: {e}"))?;
            let root = directory.path().canonicalize().map_err(|e| format!("Cannot resolve the SVG editing directory: {e}"))?;
            let file = root.join("Artwork.svg");
            let mut output = OpenOptions::new().write(true).create_new(true).open(&file).map_err(|e| format!("Cannot create the SVG editing file: {e}"))?;
            output.write_all(svg).and_then(|()| output.sync_all()).map_err(|e| format!("Cannot write the SVG editing file: {e}"))?;
            Ok(Self { directory, root, file })
        }

        /// Validate the working directory and capture a regular file identity for save settling.
        pub(super) fn stamp(&self) -> Result<Stamp, String> {
            let directory_metadata = std::fs::symlink_metadata(&self.root).map_err(|e| format!("Cannot inspect the SVG editing directory: {e}"))?;
            if !directory_metadata.is_dir() || is_redirect(&directory_metadata) || self.directory.path().canonicalize().ok().as_ref() != Some(&self.root) {
                return Err("The SVG editing directory was replaced or redirected".into());
            }
            let metadata = std::fs::symlink_metadata(&self.file).map_err(|e| format!("Cannot inspect the edited SVG: {e}"))?;
            ensure_regular(&metadata)?;
            Stamp::from_metadata(&metadata)
        }

        /// Read one settled snapshot, retaining byte limits during concurrent file growth.
        pub(super) fn read(&self, expected: &Stamp) -> Result<Vec<u8>, String> {
            if &self.stamp()? != expected {
                return Err("The SVG is still being saved".into());
            }
            let mut options = OpenOptions::new();
            options.read(true);
            no_follow(&mut options);
            let file = options.open(&self.file).map_err(|e| format!("Cannot read the edited SVG: {e}"))?;
            let metadata = file.metadata().map_err(|e| format!("Cannot inspect the edited SVG handle: {e}"))?;
            ensure_regular(&metadata)?;
            if &Stamp::from_metadata(&metadata)? != expected {
                return Err("The SVG is still being saved".into());
            }
            let size = usize::try_from(metadata.len()).map_err(|_| "The edited SVG is too large")?;
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(size).map_err(|_| "Not enough memory to read the edited SVG")?;
            // Also bound reads if a concurrent writer grows the file after the metadata check.
            file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|e| format!("Cannot read the edited SVG: {e}"))?;
            if bytes.is_empty() || bytes.len() > MAX_BYTES {
                return Err("The edited SVG must contain between 1 byte and 16 MiB".into());
            }
            if bytes.len() != size || &self.stamp()? != expected {
                return Err("The SVG is still being saved".into());
            }
            Ok(bytes)
        }
        /// Write normalized SVG through a checked, no-follow handle before editor launch.
        pub(super) fn overwrite(&self, bytes: &[u8]) -> Result<(), String> {
            if bytes.is_empty() || bytes.len() > MAX_BYTES {
                return Err("Prepared SVG exceeds the supported byte limit".into());
            }
            self.stamp()?;
            let mut options = OpenOptions::new();
            options.write(true);
            no_follow(&mut options);
            let mut file = options.open(&self.file).map_err(|e| format!("Cannot open the prepared SVG: {e}"))?;
            ensure_regular(&file.metadata().map_err(|e| format!("Cannot inspect the prepared SVG: {e}"))?)?;
            file.set_len(0).and_then(|()| file.write_all(bytes)).and_then(|()| file.sync_all()).map_err(|e| format!("Cannot write the prepared SVG: {e}"))
        }
    }

    /// Prevent redirected opens and avoid blocking on a substituted Unix FIFO.
    fn no_follow(options: &mut OpenOptions) {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Open the reparse point itself, so replacing the file with a symlink cannot
            // make this access follow it between the metadata check and opening the handle.
            options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Do not follow a replaced symlink or block opening a substituted FIFO/device.
            options.custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
        }
    }

    fn ensure_regular(metadata: &Metadata) -> Result<(), String> {
        if !metadata.is_file() || is_redirect(metadata) {
            return Err("The SVG or editor must be a regular file, not a link or reparse point".into());
        }
        Ok(())
    }

    fn is_redirect(metadata: &Metadata) -> bool {
        if metadata.file_type().is_symlink() {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x0000_0400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    #[derive(Clone, PartialEq, Eq)]
    pub(super) struct Stamp {
        modified: SystemTime,
        length: u64,
        #[cfg(unix)]
        identity: (u64, u64),
    }

    impl Stamp {
        /// Reject unsupported file sizes before capturing save time, length and native identity.
        fn from_metadata(metadata: &Metadata) -> Result<Self, String> {
            if metadata.len() == 0 || metadata.len() > MAX_BYTES as u64 {
                return Err("The edited SVG must contain between 1 byte and 16 MiB".into());
            }
            #[cfg(unix)]
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                modified: metadata.modified().map_err(|e| format!("Cannot inspect the SVG save time: {e}"))?,
                length: metadata.len(),
                #[cfg(unix)]
                identity: (metadata.dev(), metadata.ino()),
            })
        }
    }

    struct Session {
        stop: Sender<()>,
        updates: Receiver<Result<Vec<u8>, String>>,
    }

    impl ExternalSvgSession for Session {
        fn poll(&mut self) -> Option<Result<Vec<u8>, String>> {
            self.updates.try_recv().ok()
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            // No thread join or editor termination on the UI thread. The worker owns TempDir
            // and drops it after this bounded wakeup, including when the update receiver closes.
            let _ = self.stop.send(());
        }
    }

    #[cfg(test)]
    fn start(owned: OwnedSvg, initial: Vec<u8>, child: Option<Child>, interval: Duration, settle: Duration) -> Result<Session, String> {
        let (stop, stopped) = mpsc::channel();
        // At most two bounded updates await the UI, never an unbounded save queue.
        let (send, updates) = mpsc::sync_channel(2);
        thread::Builder::new()
            .name("photocraft-svg-editor".into())
            .spawn(move || watch(owned, initial, child, stopped, send, interval, settle))
            .map_err(|e| format!("Cannot watch SVG editor saves: {e}"))?;
        Ok(Session { stop, updates })
    }

    fn watch(
        owned: OwnedSvg,
        mut last_bytes: Vec<u8>,
        mut child: Option<Child>,
        stopped: Receiver<()>,
        send: SyncSender<Result<Vec<u8>, String>>,
        interval: Duration,
        settle: Duration,
    ) {
        // The editor may save before the watcher starts. First settled read must compare bytes.
        let mut last_stamp: Option<Stamp> = None;
        let mut pending: Option<(Stamp, Instant)> = None;
        let mut last_error: Option<String> = None;
        while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
            if child.as_mut().is_some_and(|process| process.try_wait().ok().flatten().is_some()) {
                child = None;
            }
            let candidate = match owned.stamp() {
                Ok(stamp) => {
                    if last_stamp.as_ref() == Some(&stamp) {
                        continue;
                    }
                    match &pending {
                        Some((old, since)) if old == &stamp && since.elapsed() >= settle => owned.read(&stamp),
                        Some((old, _)) if old == &stamp => continue,
                        _ => {
                            pending = Some((stamp, Instant::now()));
                            continue;
                        }
                    }
                }
                Err(error) => {
                    pending = None;
                    Err(error)
                }
            };
            match candidate {
                Ok(bytes) => {
                    last_error = None;
                    if bytes == last_bytes {
                        last_stamp = pending.as_ref().map(|(stamp, _)| stamp.clone());
                        continue;
                    }
                    match send.try_send(Ok(bytes.clone())) {
                        Ok(()) => {
                            last_bytes = bytes;
                            last_stamp = pending.as_ref().map(|(stamp, _)| stamp.clone());
                        }
                        Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
                Err(error) => {
                    last_stamp = None;
                    if last_error.as_ref() == Some(&error) {
                        continue;
                    }
                    match send.try_send(Err(error.clone())) {
                        Ok(()) => last_error = Some(error),
                        Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const EMPTY_SAVE: &str = "The edited SVG must contain between 1 byte and 16 MiB";
        const CHANGING_SAVE: &str = "The SVG is still being saved";

        fn update(session: &mut Session, deadline: Instant) -> Result<Vec<u8>, String> {
            loop {
                assert!(Instant::now() < deadline, "timed out waiting for an SVG save");
                if let Some(update) = session.poll() {
                    return update;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn expect_saved(session: &mut Session, expected: &[u8], deadline: Instant) {
            loop {
                match update(session, deadline) {
                    Ok(bytes) => {
                        assert_eq!(bytes, expected, "unexpected SVG save contents");
                        return;
                    }
                    Err(error) => assert!(matches!(error.as_str(), EMPTY_SAVE | CHANGING_SAVE), "unexpected SVG watcher error: {error}"),
                }
            }
        }

        fn expect_empty_error(session: &mut Session, deadline: Instant) {
            loop {
                match update(session, deadline) {
                    Err(error) if error == EMPTY_SAVE => return,
                    Err(error) if error == CHANGING_SAVE => {}
                    other => panic!("expected the held-empty SVG error, got {other:?}"),
                }
            }
        }

        fn queued_session(updates: Vec<Result<Vec<u8>, String>>) -> Session {
            let (stop, _stopped) = mpsc::channel();
            let (send, receive) = mpsc::channel();
            for update in updates {
                send.send(update).unwrap();
            }
            Session { stop, updates: receive }
        }

        #[test]
        fn saves_debounce_repeat_and_recover_after_invalid_file() {
            let initial = b"<svg/>".to_vec();
            let owned = OwnedSvg::create(&initial).unwrap();
            let file = owned.file.clone();
            let root = owned.root.clone();
            let settle = Duration::from_millis(100);
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut session = start(owned, initial, None, Duration::from_millis(10), settle).unwrap();
            let save_started = Instant::now();
            let first = b"<svg>first complete</svg>";
            std::fs::write(&file, first).unwrap();
            expect_saved(&mut session, first, deadline);
            assert!(save_started.elapsed() >= settle, "a changed save must settle before delivery");
            assert!(session.poll().is_none());
            std::fs::write(&file, b"").unwrap();
            expect_empty_error(&mut session, deadline);
            std::fs::write(&file, b"<svg>second complete</svg>").unwrap();
            expect_saved(&mut session, b"<svg>second complete</svg>", deadline);
            assert!(session.poll().is_none());
            drop(session);
            while root.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(!root.exists(), "worker must clean its owned temporary SVG");
        }

        #[test]
        fn exact_save_wait_skips_only_expected_queued_transient_errors() {
            let bytes = b"<svg>complete</svg>";
            let mut session = queued_session(vec![Err(EMPTY_SAVE.into()), Err(CHANGING_SAVE.into()), Ok(bytes.to_vec())]);
            expect_saved(&mut session, bytes, Instant::now() + Duration::from_secs(3));
            assert!(session.poll().is_none());
        }

        #[test]
        #[should_panic(expected = "unexpected SVG watcher error")]
        fn exact_save_wait_rejects_unexpected_errors() {
            let mut session = queued_session(vec![Err("Cannot read the edited SVG: permission denied".into()), Ok(b"<svg/>".to_vec())]);
            expect_saved(&mut session, b"<svg/>", Instant::now() + Duration::from_secs(3));
        }

        #[test]
        fn oversize_file_is_rejected_before_allocating() {
            let owned = OwnedSvg::create(b"<svg/>").unwrap();
            std::fs::File::create(&owned.file).unwrap().set_len(MAX_BYTES as u64 + 1).unwrap();
            assert!(owned.stamp().is_err());
            assert!(OwnedSvg::create(&[]).is_err());
        }

        #[cfg(unix)]
        #[test]
        fn atomic_replacement_with_equal_size_and_mtime_is_a_new_save() {
            let initial = b"<svg>one</svg>".to_vec();
            let owned = OwnedSvg::create(&initial).unwrap();
            let file = owned.file.clone();
            let original_stamp = owned.stamp().unwrap();
            let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
            let mut session = start(owned, initial, None, Duration::from_millis(10), Duration::from_millis(40)).unwrap();
            let replacement = file.with_extension("replacement");
            let saved = b"<svg>two</svg>";
            std::fs::write(&replacement, saved).unwrap();
            std::fs::File::options().write(true).open(&replacement).unwrap().set_modified(modified).unwrap();
            std::fs::rename(&replacement, &file).unwrap();
            let replaced_stamp = Stamp::from_metadata(&std::fs::metadata(&file).unwrap()).unwrap();
            assert_eq!(original_stamp.modified, replaced_stamp.modified);
            assert_eq!(original_stamp.length, replaced_stamp.length);
            assert!(original_stamp != replaced_stamp, "file identity must distinguish equal-time atomic saves");
            expect_saved(&mut session, saved, Instant::now() + Duration::from_secs(3));
        }

        #[cfg(unix)]
        #[test]
        fn unix_saved_file_rejects_symlinks_and_fifos_without_blocking() {
            let owned = OwnedSvg::create(b"<svg/>").unwrap();
            let stamp = owned.stamp().unwrap();
            let target = owned.root.join("private.svg");
            std::fs::write(&target, b"private contents").unwrap();
            std::fs::remove_file(&owned.file).unwrap();
            std::os::unix::fs::symlink(&target, &owned.file).unwrap();
            assert!(owned.stamp().is_err());
            assert!(owned.read(&stamp).is_err());
            assert!(owned.overwrite(b"<svg/>").is_err());
            std::fs::remove_file(&owned.file).unwrap();
            rustix::fs::mkfifoat(rustix::fs::CWD, &owned.file, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR).unwrap();
            assert!(owned.stamp().is_err());
            let began = Instant::now();
            assert!(owned.read(&stamp).is_err());
            assert!(began.elapsed() < Duration::from_secs(1));
            assert_eq!(std::fs::read(&target).unwrap(), b"private contents");
        }

        #[cfg(windows)]
        #[test]
        fn script_and_command_line_are_not_editor_executables() {
            assert!(validate_executable(Path::new("cmd.exe /c start")).is_err());
            assert!(validate_executable(Path::new("C:/editor.cmd")).is_err());
            assert!(validate_executable(Path::new("editor.exe")).is_err());
        }

        #[cfg(windows)]
        #[test]
        fn replacement_symlink_is_rejected_without_reading_target() {
            let owned = OwnedSvg::create(b"<svg/>").unwrap();
            let target = owned.root.join("other.svg");
            std::fs::write(&target, b"private target").unwrap();
            std::fs::remove_file(&owned.file).unwrap();
            // Symlink creation requires Developer Mode or elevation. Other tests run everywhere.
            if std::os::windows::fs::symlink_file(&target, &owned.file).is_err() {
                return;
            }
            assert!(owned.stamp().is_err());
            let _ = std::fs::remove_file(&owned.file);
        }
    }
}

#[path = "vector_conversion.rs"]
mod vector_conversion;
