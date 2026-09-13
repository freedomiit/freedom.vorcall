//! Where a downloaded release lives on disk, and how it takes the place of the
//! binary that is running right now.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use super::{UpdateError, Version};

/// Every file the updater writes starts with this, so a stray one is easy to
/// recognise and sweep away.
pub(crate) const PENDING_PREFIX: &str = ".vorcall-update-";

/// What happened after a successful swap. On unix `apply_and_relaunch` never
/// returns on success — the process image is replaced.
#[derive(Clone, Copy, Debug)]
pub enum Relaunched {
    Spawned,
}

/// The running binary, with every symlink resolved: the file a swap replaces.
pub fn current_exe_path() -> io::Result<PathBuf> {
    fs::canonicalize(std::env::current_exe()?)
}

pub fn install_dir() -> io::Result<PathBuf> {
    let exe = current_exe_path()?;
    exe.parent().map(Path::to_path_buf).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the running binary has no parent directory",
        )
    })
}

pub fn pending_path(dir: &Path, version: &Version) -> PathBuf {
    dir.join(format!("{PENDING_PREFIX}{version}"))
}

/// The manifest the pending download was verified against.
pub fn manifest_path(pending: &Path) -> PathBuf {
    with_suffix(pending, ".manifest.json")
}

pub fn signature_path(pending: &Path) -> PathBuf {
    with_suffix(pending, ".manifest.sig")
}

/// Where bytes land while they are still arriving.
pub fn part_path(pending: &Path) -> PathBuf {
    with_suffix(pending, ".part")
}

/// `Path::with_extension` would eat the patch number: the file name already
/// contains dots.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Creates the file, failing if it is already there, so two updaters never
/// write the same bytes over each other.
///
/// The leading dot of [`PENDING_PREFIX`] is the only unobtrusiveness these
/// files get: a Windows hidden attribute would ride the renames of a swap and
/// leave the installed binary hidden.
pub fn create_temp(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Reserves the download's `.part` file next to the installed binary, falling
/// back to the user's local data directory when the install directory is not
/// writable (a system-wide install). The `bool` is that fallback: such a
/// download is never applied automatically.
pub fn writable_target(
    dir: &Path,
    version: &Version,
) -> Result<(File, PathBuf, bool), UpdateError> {
    let part = part_path(&pending_path(dir, version));

    match create_fresh(&part) {
        Ok(file) => return Ok((file, part, false)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
            ) => {}
        Err(e) => return Err(UpdateError::Io(e)),
    }

    let fallback = fallback_dir()?;
    fs::create_dir_all(&fallback)?;
    let part = part_path(&pending_path(&fallback, version));

    Ok((create_fresh(&part)?, part, true))
}

/// A `.part` left behind by a run that died mid-download carries no value: its
/// bytes were never hash-checked.
fn create_fresh(path: &Path) -> io::Result<File> {
    match create_temp(path) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            create_temp(path)
        }
        other => other,
    }
}

fn fallback_dir() -> Result<PathBuf, UpdateError> {
    // Same load-bearing strings as `config::log_dir` — see that comment.
    directories::ProjectDirs::from("br.com", "freedomit", "vorcall")
        .map(|dirs| dirs.data_local_dir().join("updates"))
        .ok_or_else(|| {
            UpdateError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "this platform exposes no local data directory",
            ))
        })
}

/// Puts `pending` in the place of the running binary and starts it again.
///
/// Renaming over a running binary is safe on unix: the kernel keeps the open
/// image alive for this process, and the exec below runs the new inode.
#[cfg(unix)]
pub fn apply_and_relaunch(pending: &Path, args: &[OsString]) -> Result<Relaunched, UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    let exe = current_exe_path()?;
    let mode = fs::metadata(&exe)?.permissions().mode() | 0o755;
    fs::set_permissions(pending, fs::Permissions::from_mode(mode))?;

    fs::rename(pending, &exe)
        .map_err(|e| UpdateError::Swap(format!("cannot replace {}: {e}", exe.display())))?;
    let _ = fs::remove_file(manifest_path(pending));
    let _ = fs::remove_file(signature_path(pending));

    // `exec` only returns when it failed; the new binary is already in place.
    let error = std::process::Command::new(&exe)
        .args(args)
        .env("VORCALL_RELAUNCHED", "1")
        .exec();
    Err(UpdateError::Swap(format!(
        "relaunch failed after swap, start Vorcall again by hand: {error}"
    )))
}

/// Windows locks the image of a running process, so the old binary is moved
/// aside instead of overwritten, and swept away by [`cleanup_old`] next start.
#[cfg(windows)]
pub fn apply_and_relaunch(pending: &Path, args: &[OsString]) -> Result<Relaunched, UpdateError> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let exe = current_exe_path()?;
    let old = exe.with_extension("exe.old");
    let _ = fs::remove_file(&old);

    fs::rename(&exe, &old)
        .map_err(|e| UpdateError::Swap(format!("cannot move the current binary aside: {e}")))?;
    if let Err(e) = fs::rename(pending, &exe) {
        let _ = fs::rename(&old, &exe);
        return Err(UpdateError::Swap(format!(
            "cannot install the download: {e}"
        )));
    }
    let _ = fs::remove_file(manifest_path(pending));
    let _ = fs::remove_file(signature_path(pending));

    std::process::Command::new(&exe)
        .args(args)
        .env("VORCALL_RELAUNCHED", "1")
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map_err(|e| {
            UpdateError::Swap(format!(
                "the update is installed but it did not start, run Vorcall again: {e}"
            ))
        })?;

    Ok(Relaunched::Spawned)
}

/// Removes the binary the last Windows swap moved aside. The previous process
/// may still be exiting and holding it, so a failure is not worth reporting:
/// the next start tries again.
pub fn cleanup_old() {
    #[cfg(windows)]
    if let Ok(exe) = current_exe_path() {
        let _ = fs::remove_file(exe.with_extension("exe.old"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::testing;

    fn version() -> Version {
        "1.2.3".parse().expect("should parse")
    }

    #[test]
    fn side_files_keep_the_whole_version() {
        let pending = pending_path(Path::new("/opt/vorcall"), &version());

        assert!(pending.ends_with(".vorcall-update-1.2.3"));
        assert!(manifest_path(&pending).ends_with(".vorcall-update-1.2.3.manifest.json"));
        assert!(signature_path(&pending).ends_with(".vorcall-update-1.2.3.manifest.sig"));
        assert!(part_path(&pending).ends_with(".vorcall-update-1.2.3.part"));
    }

    #[test]
    fn a_stale_part_is_replaced() {
        let dir = testing::temp_dir("swap");
        let part = part_path(&pending_path(&dir, &version()));
        fs::write(&part, b"half a download").expect("should write");

        let (file, reserved, manual) =
            writable_target(&dir, &version()).expect("the directory is writable");
        drop(file);

        assert_eq!(reserved, part);
        assert!(!manual);
        assert_eq!(
            fs::metadata(&part).expect("should exist").len(),
            0,
            "the stale bytes should be gone"
        );

        testing::remove_dir(&dir);
    }

    #[test]
    fn create_temp_refuses_an_existing_file() {
        let dir = testing::temp_dir("swap-exclusive");
        let path = dir.join("once");

        drop(create_temp(&path).expect("the first create should work"));
        let error = create_temp(&path).expect_err("the second create should fail");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        testing::remove_dir(&dir);
    }

    #[test]
    fn the_install_dir_is_the_parent_of_the_running_binary() {
        let dir = install_dir().expect("the test binary has a parent");
        let exe = current_exe_path().expect("the test binary exists");

        assert_eq!(exe.parent().expect("has a parent"), dir);
    }
}
