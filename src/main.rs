//! proton-ntfs-fix
//!
//! Scans a Steam library on NTFS and makes sure every "compatdata"
//! folder (Proton prefix) is a symlink to a native Linux filesystem
//! instead of a real directory on NTFS.
//!
//! Background: ntfs3/ntfs-3g don't reliably support the POSIX
//! symlinks that Proton needs when creating a prefix
//! (e.g. dosdevices/c: -> ../drive_c). The game files themselves can
//! stay on NTFS without any issue -- only the prefix needs to live on
//! ext4/btrfs/etc.
//!
//! Steam only ever creates a compatdata folder for titles that run
//! through Proton -- native Linux games never get a compatdata entry
//! at all. On top of that, the following are filtered out:
//!   - the folder "0" (shared/legacy prefix, not a real game)
//!   - AppIDs without a matching appmanifest_<AppID>.acf (typically
//!     runtime helper tools/redistributables rather than real games)
//!
//! The actual move does NOT go through fs::rename, because rename(2)
//! only works within the same filesystem -- and the NTFS library vs.
//! the native base directory are by definition two different mounts.
//! Instead: recursively copy into a temporary directory on the native
//! FS, verify the entry count, atomically rename it (now same
//! filesystem) to the final name, and only then remove the original
//! on NTFS and create the symlink.
//!
//! Usage:
//!   proton-ntfs-fix [--dry-run] [--yes] [<AppID|all>] [ntfs-library] [native-base-dir]
//!
//!   --dry-run   only shows what would be done, doesn't change anything
//!   --yes, -y   skips the confirmation prompt before moving

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};

fn default_ntfs_library() -> PathBuf {
    PathBuf::from("/mnt/ntfs/SteamLibrary")
}

fn default_native_base() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join(".local/share/steam-compatdata")
}

struct Config {
    mode: String, // "all" or a specific AppID
    ntfs_library: PathBuf,
    native_base: PathBuf,
    dry_run: bool,
    assume_yes: bool,
}

fn parse_args() -> Config {
    let mut dry_run = false;
    let mut assume_yes = false;
    let mut positional = Vec::new();

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--yes" | "-y" => assume_yes = true,
            other => positional.push(other.to_string()),
        }
    }

    let mode = positional.first().cloned().unwrap_or_else(|| "all".to_string());
    let ntfs_library = positional
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_ntfs_library);
    let native_base = positional
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(default_native_base);

    Config {
        mode,
        ntfs_library,
        native_base,
        dry_run,
        assume_yes,
    }
}

/// Checks whether an AppID is a real, Proton-run game.
fn is_real_game(appid: &str, ntfs_library: &Path) -> bool {
    if appid == "0" {
        return false;
    }
    let manifest = ntfs_library
        .join("steamapps")
        .join(format!("appmanifest_{appid}.acf"));
    manifest.is_file()
}

enum Status {
    AlreadyOk,
    WrongTarget(PathBuf),
    NeedsMove,
    Skipped(&'static str),
    Error(String),
}

/// Pure inventory check -- doesn't change anything on disk.
fn plan_appid(appid: &str, cfg: &Config) -> Status {
    if !is_real_game(appid, &cfg.ntfs_library) {
        return Status::Skipped("not a real game (runtime tool/legacy prefix)");
    }

    let compatdata_root = cfg.ntfs_library.join("steamapps").join("compatdata");
    let ntfs_dir = compatdata_root.join(appid);
    let native_dir = cfg.native_base.join(appid);

    match fs::symlink_metadata(&ntfs_dir) {
        Ok(m) if m.file_type().is_symlink() => match fs::canonicalize(&ntfs_dir) {
            Ok(target) => {
                let expected = fs::canonicalize(&native_dir).unwrap_or_else(|_| native_dir.clone());
                if target == expected {
                    Status::AlreadyOk
                } else {
                    Status::WrongTarget(target)
                }
            }
            Err(e) => Status::Error(format!("symlink broken/unresolvable: {e}")),
        },
        Ok(m) if m.is_dir() => {
            if native_dir.exists() {
                Status::Error(format!(
                    "native target directory already exists ({}), but NTFS folder is not a symlink -- check manually",
                    native_dir.display()
                ))
            } else {
                Status::NeedsMove
            }
        }
        Ok(_) => Status::Error("unexpected file type".to_string()),
        Err(_) => Status::Skipped("no folder present"),
    }
}

/// Recursively copies a directory tree; symlinks stay symlinks (even
/// broken ones -- we only copy the link target, without resolving it).
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_symlink() {
            let target = fs::read_link(&src_path)?;
            unix_fs::symlink(&target, &dst_path)?;
        } else if file_type.is_dir() {
            copy_tree(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path)?;
            let perms = fs::metadata(&src_path)?.permissions();
            fs::set_permissions(&dst_path, perms)?;
        }
        // Sockets/FIFOs etc. aren't expected inside a Proton prefix
        // and are deliberately skipped.
    }
    Ok(())
}

/// Recursively counts files+directories -- serves as a simple
/// plausibility check after copying (not a substitute for a real
/// checksum verification, but reliably catches aborted copies).
fn count_entries(path: &Path) -> io::Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        count += 1;
        if entry.file_type()?.is_dir() {
            count += count_entries(&entry.path())?;
        }
    }
    Ok(count)
}

/// Safely moves a prefix from NTFS to a native filesystem, even
/// though both live on different mounts (fs::rename would fail here
/// with EXDEV).
fn move_prefix(ntfs_dir: &Path, native_dir: &Path) -> io::Result<()> {
    let parent = native_dir
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    fs::create_dir_all(parent)?;

    let tmp_name = format!(
        "{}.proton-ntfs-fix-tmp-{}",
        native_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("prefix"),
        process::id()
    );
    let tmp_dir = parent.join(tmp_name);
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir)?;
    }

    if let Err(e) = copy_tree(ntfs_dir, &tmp_dir) {
        fs::remove_dir_all(&tmp_dir).ok();
        return Err(e);
    }

    let src_count = count_entries(ntfs_dir)?;
    let dst_count = count_entries(&tmp_dir)?;
    if src_count != dst_count {
        fs::remove_dir_all(&tmp_dir).ok();
        return Err(io::Error::other(format!(
            "verification failed: {src_count} entries in source, {dst_count} in copy -- original left untouched"
        )));
    }

    // From here on: tmp_dir is on the same FS as native_dir -> atomic.
    if let Err(e) = fs::rename(&tmp_dir, native_dir) {
        fs::remove_dir_all(&tmp_dir).ok();
        return Err(e);
    }

    // Don't remove the original right away -- first move it aside
    // (same FS, atomic). If creating the symlink below fails, we can
    // just rename it back instead of losing data -- the native
    // directory is then left as a (detectable) duplicate, and the
    // next run reports that via the "already exists" case in
    // plan_appid instead of silently orphaning data.
    let backup_dir = ntfs_dir.with_file_name(format!(
        "{}.proton-ntfs-fix-backup",
        ntfs_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("prefix")
    ));
    if backup_dir.exists() {
        fs::remove_dir_all(&backup_dir)?;
    }
    fs::rename(ntfs_dir, &backup_dir)?;

    if let Err(e) = unix_fs::symlink(native_dir, ntfs_dir) {
        fs::rename(&backup_dir, ntfs_dir).ok();
        return Err(e);
    }

    fs::remove_dir_all(&backup_dir)?;

    Ok(())
}

fn list_appids(compatdata_root: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    let Ok(entries) = fs::read_dir(compatdata_root) else {
        return ids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.chars().all(|c| c.is_ascii_digit()) {
            ids.push(name.to_string());
        }
    }
    ids.sort_by_key(|s| s.parse::<u64>().unwrap_or(0));
    ids
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt} [y/N]: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

fn main() -> ExitCode {
    let cfg = parse_args();
    let compatdata_root = cfg.ntfs_library.join("steamapps").join("compatdata");

    println!("== Steam NTFS Prefix Fix (Rust) ==");
    println!("NTFS library:      {}", cfg.ntfs_library.display());
    println!("Native base dir:   {}", cfg.native_base.display());
    if cfg.dry_run {
        println!("Mode:              DRY RUN (no changes will be made)");
    }
    println!();

    if !compatdata_root.is_dir() {
        eprintln!(
            "ERROR: '{}' does not exist. Wrong library path given?",
            compatdata_root.display()
        );
        return ExitCode::FAILURE;
    }

    if !cfg.dry_run && let Err(e) = fs::create_dir_all(&cfg.native_base) {
        eprintln!("ERROR: could not create native base directory: {e}");
        return ExitCode::FAILURE;
    }

    let appids: Vec<String> = if cfg.mode == "all" {
        list_appids(&compatdata_root)
    } else {
        vec![cfg.mode.clone()]
    };

    if appids.is_empty() {
        println!("No AppIDs found in {}.", compatdata_root.display());
        return ExitCode::SUCCESS;
    }

    let mut had_error = false;
    let mut pending: Vec<String> = Vec::new();

    for appid in &appids {
        match plan_appid(appid, &cfg) {
            Status::AlreadyOk => println!("[OK]      {appid:<10} symlink already correct"),
            Status::WrongTarget(target) => println!(
                "[WARNING] {appid:<10} symlink points to unexpected target ({}), ignoring",
                target.display()
            ),
            Status::Skipped(reason) => println!("[SKIP]    {appid:<10} {reason}"),
            Status::Error(msg) => {
                eprintln!("[ERROR]   {appid:<10} {msg}");
                had_error = true;
            }
            Status::NeedsMove => pending.push(appid.clone()),
        }
    }

    if pending.is_empty() {
        println!();
        println!("Done -- nothing to move.");
        return if had_error { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }

    println!();
    println!("The following prefixes need to be moved from NTFS to a native filesystem:");
    for appid in &pending {
        let native_dir = cfg.native_base.join(appid);
        println!("  - {appid} -> {}", native_dir.display());
    }
    println!();

    if cfg.dry_run {
        println!("DRY RUN: no changes made.");
        return if had_error { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }

    if !cfg.assume_yes && !confirm("Move now?") {
        println!("Aborted.");
        return ExitCode::SUCCESS;
    }

    println!();
    for appid in &pending {
        let ntfs_dir = compatdata_root.join(appid);
        let native_dir = cfg.native_base.join(appid);
        match move_prefix(&ntfs_dir, &native_dir) {
            Ok(()) => println!(
                "[FIXED]   {appid:<10} prefix moved to {} and linked",
                native_dir.display()
            ),
            Err(e) => {
                eprintln!("[ERROR]   {appid:<10} move failed: {e}");
                had_error = true;
            }
        }
    }

    println!();
    println!("Done.");

    if had_error {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
