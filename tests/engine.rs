use dircrypt::{Options, execute};
use rusqlite::Connection;
#[cfg(unix)]
use std::os::unix::fs::{symlink as symlink_dir, symlink as symlink_file};
#[cfg(windows)]
use std::os::windows::fs::{symlink_dir, symlink_file};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn options(header: bool) -> Options {
    Options {
        obfuscate: header,
        quiet: true,
        jobs: 2,
        ..Default::default()
    }
}
fn put(root: &Path, name: &str, bytes: &[u8]) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}
fn fixture(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut expected = BTreeMap::new();
    for (i, name) in [
        "plain.txt",
        "node_modules/.bin/run",
        "node_modules/.pnpm/pkg@1.0/node_modules/pkg/index.js",
        "node_modules/@scope/deep/package.json",
        "中文/嵌套/测试.bin",
        ".hidden/.dot",
        "empty",
        "sixteen",
        "seventeen",
    ]
    .iter()
    .enumerate()
    {
        let size = match i {
            6 => 0,
            7 => 16,
            8 => 17,
            _ => 128 + i,
        };
        let bytes = (0..size).map(|n| n as u8).collect::<Vec<_>>();
        put(root, name, &bytes);
        expected.insert(name.to_string(), bytes);
    }
    fs::create_dir_all(root.join("empty-directory/also-empty")).unwrap();
    expected
}
fn verify(root: &Path, expected: &BTreeMap<String, Vec<u8>>) {
    for (name, bytes) in expected {
        assert_eq!(fs::read(root.join(name)).unwrap(), *bytes, "{name}");
    }
    assert!(root.join("empty-directory/also-empty").is_dir());
    assert!(!root.join("DCDATA").exists());
}
fn cli(root: &Path, flags: &[&str], crash: Option<(&str, usize)>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dircrypt"));
    command
        .arg(root)
        .arg("--quiet")
        .arg("--jobs")
        .arg("1")
        .args(flags);
    if let Some((point, after)) = crash {
        command
            .env("DIRCRYPT_TEST_CRASH_POINT", point)
            .env("DIRCRYPT_TEST_CRASH_AFTER", after.to_string());
    }
    command.output().unwrap()
}
fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn mapped_file(root: &Path) -> PathBuf {
    let mut pending = vec![root.join("DCDATA/data")];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                return entry.path();
            }
        }
    }
    panic!("No stored file")
}

#[test]
fn round_trip_hidden_deep_unicode_empty_and_header_boundaries() {
    for header in [false, true] {
        let temp = TempDir::new().unwrap();
        let expected = fixture(temp.path());
        let mapped = execute(temp.path(), options(header)).unwrap();
        assert_eq!(mapped.files, expected.len() as u64);
        let restored = execute(temp.path(), options(!header)).unwrap();
        assert_eq!(restored.action, "restored");
        verify(temp.path(), &expected);
    }
}

#[test]
fn mapping_does_not_change_file_bytes() {
    let temp = TempDir::new().unwrap();
    let bytes = b"contents must stay byte-identical";
    put(temp.path(), "single", bytes);
    execute(temp.path(), options(false)).unwrap();
    assert_eq!(fs::read(mapped_file(temp.path())).unwrap(), bytes);
    execute(temp.path(), options(false)).unwrap();
    assert_eq!(fs::read(temp.path().join("single")).unwrap(), bytes);
}

#[test]
fn real_process_crashes_recover_at_each_boundary() {
    for header in [false, true] {
        for (point, after) in [
            ("plan", 1),
            ("prepared", 1),
            ("rename", 1),
            ("rename", 5),
            ("rename", 12),
        ] {
            let temp = TempDir::new().unwrap();
            let expected = fixture(temp.path());
            let output = cli(
                temp.path(),
                if header { &["--obfuscate"] } else { &[] },
                Some((point, after)),
            );
            assert_eq!(
                output.status.code(),
                Some(77),
                "{point}/{after}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            success(cli(temp.path(), &[], None));
            verify(temp.path(), &expected);
        }
    }
}

#[test]
fn header_crash_and_repeated_restore_crash_are_idempotent() {
    let temp = TempDir::new().unwrap();
    let expected = fixture(temp.path());
    assert_eq!(
        cli(temp.path(), &["--obfuscate"], Some(("header", 3)))
            .status
            .code(),
        Some(77)
    );
    assert_eq!(
        cli(temp.path(), &[], Some(("rename", 1))).status.code(),
        Some(77)
    );
    success(cli(temp.path(), &[], None));
    verify(temp.path(), &expected);
}

#[test]
fn cleanup_crash_finishes_without_starting_another_mapping() {
    for point in ["cleanup", "cleanup_data"] {
        let temp = TempDir::new().unwrap();
        let expected = fixture(temp.path());
        success(cli(temp.path(), &[], None));
        assert_eq!(
            cli(temp.path(), &[], Some((point, 1))).status.code(),
            Some(77)
        );
        success(cli(temp.path(), &[], None));
        verify(temp.path(), &expected);
    }
}

#[test]
fn partial_header_writes_are_repaired_from_backup() {
    for restoring in [false, true] {
        let temp = TempDir::new().unwrap();
        let expected = fixture(temp.path());
        if restoring {
            success(cli(temp.path(), &["--obfuscate"], None));
        }
        assert_eq!(
            cli(temp.path(), &["--obfuscate"], Some(("header_partial", 1)))
                .status
                .code(),
            Some(77)
        );
        success(cli(temp.path(), &[], None));
        verify(temp.path(), &expected);
    }
}

#[test]
fn deleting_a_record_is_detected_before_restoration() {
    let temp = TempDir::new().unwrap();
    fixture(temp.path());
    execute(temp.path(), options(true)).unwrap();
    let stored = mapped_file(temp.path());
    let before = fs::read(&stored).unwrap();
    let conn = Connection::open(temp.path().join("DCDATA/state.sqlite3")).unwrap();
    conn.execute(
        "DELETE FROM nodes WHERE kind=0 AND id=(SELECT max(id) FROM nodes WHERE kind=0)",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(execute(temp.path(), options(false)).is_err());
    assert_eq!(fs::read(stored).unwrap(), before);
}

#[test]
fn long_paths_and_deep_directories_round_trip() {
    let temp = TempDir::new().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let relative = (0..70)
        .map(|i| format!("level-{i:02}"))
        .collect::<Vec<_>>()
        .join("/");
    put(
        &root,
        &format!("{relative}/file"),
        b"deeply nested data, preserved exactly",
    );
    execute(&root, options(true)).unwrap();
    execute(&root, options(false)).unwrap();
    assert_eq!(
        fs::read(root.join(relative).join("file")).unwrap(),
        b"deeply nested data, preserved exactly"
    );
}

#[test]
fn lock_contention_and_lock_symlinks_fail_without_processing() {
    let temp = TempDir::new().unwrap();
    put(temp.path(), "file", b"untouched");
    let lock = fs::File::create(temp.path().join(".dircrypt.lock")).unwrap();
    lock.lock().unwrap();
    assert!(!cli(temp.path(), &[], None).status.success());
    assert!(!temp.path().join("DCDATA").exists());
    drop(lock);
    fs::remove_file(temp.path().join(".dircrypt.lock")).unwrap();
    symlink_file(temp.path().join("file"), temp.path().join(".dircrypt.lock")).unwrap();
    assert!(execute(temp.path(), options(false)).is_err());
    assert_eq!(fs::read(temp.path().join("file")).unwrap(), b"untouched");
}

#[test]
fn database_symlink_and_directory_substitution_are_rejected() {
    let temp = TempDir::new().unwrap();
    let expected = fixture(temp.path());
    execute(temp.path(), options(true)).unwrap();
    let database = temp.path().join("DCDATA/state.sqlite3");
    let saved = temp.path().join("saved.sqlite3");
    fs::rename(&database, &saved).unwrap();
    symlink_file(&saved, &database).unwrap();
    assert!(execute(temp.path(), options(false)).is_err());
    fs::remove_file(&database).unwrap();
    fs::rename(saved, database).unwrap();
    execute(temp.path(), options(false)).unwrap();
    verify(temp.path(), &expected);
    execute(temp.path(), options(false)).unwrap();
    let stored = fs::read_dir(temp.path().join("DCDATA/data"))
        .unwrap()
        .map(|v| v.unwrap())
        .find(|v| v.file_type().unwrap().is_dir())
        .unwrap()
        .path();
    let outside = temp.path().join("moved-out");
    fs::rename(&stored, &outside).unwrap();
    symlink_dir(&outside, &stored).unwrap();
    assert!(execute(temp.path(), options(false)).is_err());
    assert!(outside.is_dir());
}

#[test]
fn cancellation_retains_a_plan_and_all_original_bytes() {
    use std::sync::atomic::Ordering;
    let temp = TempDir::new().unwrap();
    let expected = fixture(temp.path());
    let opt = options(true);
    opt.cancelled.store(true, Ordering::Relaxed);
    assert!(execute(temp.path(), opt).is_err());
    assert!(temp.path().join("DCDATA/state.sqlite3").is_file());
    execute(temp.path(), options(false)).unwrap();
    verify(temp.path(), &expected);
}

#[test]
fn reports_never_overwrite_or_enter_the_target_tree() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    put(&root, "file", b"preserve");
    let report = temp.path().join("report.json");
    fs::write(&report, b"existing report").unwrap();
    assert!(
        !cli(&root, &["--report", report.to_str().unwrap()], None)
            .status
            .success()
    );
    assert_eq!(fs::read(report).unwrap(), b"existing report");
    assert!(!root.join("DCDATA").exists());
    assert!(
        !cli(
            &root,
            &["--report", root.join("new.json").to_str().unwrap()],
            None
        )
        .status
        .success()
    );
    assert!(!root.join("new.json").exists());
}

#[test]
fn unrelated_sqlite_database_is_not_reconfigured_or_modified() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("DCDATA/state.sqlite3");
    fs::create_dir(path.parent().unwrap()).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE valuable(data TEXT); INSERT INTO valuable VALUES('keep');").unwrap();
    drop(conn);
    let before = fs::read(&path).unwrap();
    assert!(execute(temp.path(), options(false)).is_err());
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn unknown_control_files_keep_the_database_during_cleanup() {
    let temp = TempDir::new().unwrap();
    let expected = fixture(temp.path());
    execute(temp.path(), options(false)).unwrap();
    put(temp.path(), "DCDATA/foreign", b"preserve this too");
    assert!(execute(temp.path(), options(false)).is_err());
    assert!(temp.path().join("DCDATA/state.sqlite3").is_file());
    assert_eq!(
        fs::read(temp.path().join("DCDATA/foreign")).unwrap(),
        b"preserve this too"
    );
    fs::remove_file(temp.path().join("DCDATA/foreign")).unwrap();
    execute(temp.path(), options(false)).unwrap();
    verify(temp.path(), &expected);
}

#[test]
fn checkpoint_crash_in_wide_directory_recovers_all_files() {
    let temp = TempDir::new().unwrap();
    for i in 0i32..4200 {
        put(temp.path(), &format!("{i}.bin"), &i.to_le_bytes());
    }
    assert_eq!(
        cli(temp.path(), &[], Some(("checkpoint", 1))).status.code(),
        Some(77)
    );
    success(cli(temp.path(), &[], None));
    for i in 0i32..4200 {
        assert_eq!(
            fs::read(temp.path().join(format!("{i}.bin"))).unwrap(),
            i.to_le_bytes()
        );
    }
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
            // fs::copy does not promise timestamp preservation (notably empty
            // files on exFAT). Model a timestamp-preserving archive transfer.
            fs::File::options()
                .write(true)
                .open(target)
                .unwrap()
                .set_modified(entry.metadata().unwrap().modified().unwrap())
                .unwrap();
        }
    }
}

#[test]
fn complete_archive_can_be_copied_to_another_root() {
    for header in [false, true] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let expected = fixture(&source);
        execute(&source, options(header)).unwrap();
        let target = temp.path().join("copy");
        copy_tree(&source, &target);
        execute(&target, options(false)).unwrap();
        verify(&target, &expected);
        execute(&source, options(false)).unwrap();
        verify(&source, &expected);
    }
}

#[test]
fn copied_incomplete_archive_fails_without_changes() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fixture(&source);
    assert_eq!(
        cli(&source, &["--obfuscate"], Some(("rename", 5)))
            .status
            .code(),
        Some(77)
    );
    let target = temp.path().join("copy");
    copy_tree(&source, &target);
    assert!(
        execute(&target, options(false))
            .unwrap_err()
            .to_string()
            .contains("incomplete archive")
    );
    assert!(target.join("DCDATA/state.sqlite3").exists());
    success(cli(&source, &[], None));
}

#[test]
fn missing_and_replaced_files_preserve_the_recovery_database() {
    let temp = TempDir::new().unwrap();
    put(temp.path(), "file", b"valuable bytes");
    execute(temp.path(), options(false)).unwrap();
    let stored = mapped_file(temp.path());
    let saved = temp.path().join("saved");
    fs::rename(&stored, &saved).unwrap();
    assert!(execute(temp.path(), options(false)).is_err());
    assert!(temp.path().join("DCDATA/state.sqlite3").exists());
    fs::copy(&saved, &stored).unwrap();
    assert!(execute(temp.path(), options(false)).is_err());
    fs::remove_file(&stored).unwrap();
    fs::rename(saved, stored).unwrap();
    execute(temp.path(), options(false)).unwrap();
    assert_eq!(
        fs::read(temp.path().join("file")).unwrap(),
        b"valuable bytes"
    );
}

#[test]
#[ignore = "Set DIRCRYPT_TEST_VOLUME to a second local filesystem"]
fn copy_complete_archive_to_another_filesystem() {
    let volume =
        std::env::var_os("DIRCRYPT_TEST_VOLUME").expect("DIRCRYPT_TEST_VOLUME is required");
    for header in [false, true] {
        let source = TempDir::new().unwrap();
        let expected = fixture(source.path());
        execute(source.path(), options(header)).unwrap();
        let target = TempDir::new_in(&volume).unwrap();
        copy_tree(source.path(), target.path());
        execute(target.path(), options(false)).unwrap();
        verify(target.path(), &expected);
        execute(source.path(), options(false)).unwrap();
        verify(source.path(), &expected);
    }
}

#[test]
#[cfg(windows)]
fn briefly_locked_file_is_retried_and_persistent_lock_retains_plan() {
    use std::os::windows::fs::OpenOptionsExt;
    let temp = TempDir::new().unwrap();
    put(temp.path(), "file", b"locked payload, preserved bytes");
    let lock = fs::File::options()
        .read(true)
        .share_mode(1)
        .open(temp.path().join("file"))
        .unwrap();
    assert!(execute(temp.path(), options(false)).is_err());
    assert!(temp.path().join("DCDATA/state.sqlite3").exists());
    drop(lock);
    execute(temp.path(), options(false)).unwrap();
    let lock = fs::File::options()
        .read(true)
        .share_mode(1)
        .open(temp.path().join("file"))
        .unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(80));
        drop(lock);
    });
    execute(temp.path(), options(false)).unwrap();
    release.join().unwrap();
    execute(temp.path(), options(false)).unwrap();
    assert_eq!(
        fs::read(temp.path().join("file")).unwrap(),
        b"locked payload, preserved bytes"
    );
}

#[test]
fn unrelated_destination_is_never_overwritten() {
    let temp = TempDir::new().unwrap();
    put(temp.path(), "file", b"original payload 0123456789");
    execute(temp.path(), options(true)).unwrap();
    let stored = mapped_file(temp.path());
    let bytes = fs::read(&stored).unwrap();
    put(temp.path(), "file", b"unrelated");
    assert!(execute(temp.path(), options(false)).is_err());
    assert_eq!(fs::read(temp.path().join("file")).unwrap(), b"unrelated");
    assert_eq!(fs::read(stored).unwrap(), bytes);
    fs::remove_file(temp.path().join("file")).unwrap();
    execute(temp.path(), options(false)).unwrap();
    assert_eq!(
        fs::read(temp.path().join("file")).unwrap(),
        b"original payload 0123456789"
    );
}

#[test]
fn unexpected_storage_files_survive_cleanup() {
    let temp = TempDir::new().unwrap();
    let expected = fixture(temp.path());
    execute(temp.path(), options(false)).unwrap();
    put(temp.path(), "DCDATA/data/foreign", b"preserve");
    assert!(execute(temp.path(), options(false)).is_err());
    assert_eq!(
        fs::read(temp.path().join("DCDATA/data/foreign")).unwrap(),
        b"preserve"
    );
    fs::remove_file(temp.path().join("DCDATA/data/foreign")).unwrap();
    execute(temp.path(), options(false)).unwrap();
    verify(temp.path(), &expected);
}

#[test]
fn corrupt_record_fails_before_any_move_or_header_write() {
    let temp = TempDir::new().unwrap();
    fixture(temp.path());
    execute(temp.path(), options(true)).unwrap();
    let stored = mapped_file(temp.path());
    let before = fs::read(&stored).unwrap();
    let conn = Connection::open(temp.path().join("DCDATA/state.sqlite3")).unwrap();
    conn.execute(
        "UPDATE nodes SET header=zeroblob(16) WHERE header IS NOT NULL",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(execute(temp.path(), options(false)).is_err());
    assert_eq!(fs::read(stored).unwrap(), before);
}

#[test]
fn external_header_edits_are_not_overwritten_by_recovery() {
    use std::io::Write;
    let temp = TempDir::new().unwrap();
    put(temp.path(), "file", b"original header and immutable body");
    execute(temp.path(), options(true)).unwrap();
    let stored = mapped_file(temp.path());
    let mut file = fs::File::options().write(true).open(&stored).unwrap();
    file.write_all(&[0xee; 16]).unwrap();
    drop(file);
    let before = fs::read(&stored).unwrap();
    assert!(execute(temp.path(), options(false)).is_err());
    assert_eq!(fs::read(stored).unwrap(), before);
    assert!(temp.path().join("DCDATA/state.sqlite3").exists());
}

#[test]
fn old_format_is_rejected_without_modification() {
    let temp = TempDir::new().unwrap();
    put(temp.path(), "DCDATA/old-file", b"valuable");
    assert!(execute(temp.path(), options(false)).is_err());
    assert_eq!(
        fs::read(temp.path().join("DCDATA/old-file")).unwrap(),
        b"valuable"
    );
}

#[test]
fn same_volume_root_rename_and_other_working_directory() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("before");
    fs::create_dir(&root).unwrap();
    let expected = fixture(&root);
    execute(&root, options(true)).unwrap();
    let after = temp.path().join("after");
    fs::rename(root, &after).unwrap();
    success(
        Command::new(env!("CARGO_BIN_EXE_dircrypt"))
            .arg(&after)
            .arg("--quiet")
            .current_dir(temp.path())
            .output()
            .unwrap(),
    );
    verify(&after, &expected);
}

#[test]
fn wide_directory_is_partitioned_and_round_trips() {
    let temp = TempDir::new().unwrap();
    for i in 0i32..4100 {
        put(temp.path(), &format!("{i}.bin"), &i.to_le_bytes());
    }
    execute(temp.path(), options(false)).unwrap();
    let entries = fs::read_dir(temp.path().join("DCDATA/data"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 5);
    assert!(entries.iter().all(|e| e.file_type().unwrap().is_dir()));
    execute(temp.path(), options(false)).unwrap();
    for i in 0i32..4100 {
        assert_eq!(
            fs::read(temp.path().join(format!("{i}.bin"))).unwrap(),
            i.to_le_bytes()
        );
    }
}

#[test]
fn symlink_cycle_and_external_target_are_not_traversed() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("data"), b"external").unwrap();
    put(&root, "node_modules/pkg/actual", b"inside");
    symlink_dir(&root, root.join("cycle")).unwrap();
    symlink_file(outside.join("data"), root.join("linked")).unwrap();
    execute(&root, options(true)).unwrap();
    assert_eq!(fs::read(outside.join("data")).unwrap(), b"external");
    execute(&root, options(false)).unwrap();
    assert_eq!(fs::read_link(root.join("cycle")).unwrap(), root);
    assert_eq!(fs::read(root.join("linked")).unwrap(), b"external");
}

#[test]
fn hard_links_are_preserved_and_header_mode_refuses_them() {
    let temp = TempDir::new().unwrap();
    put(temp.path(), "first", b"a sufficiently long payload");
    fs::hard_link(temp.path().join("first"), temp.path().join("second")).unwrap();
    assert!(execute(temp.path(), options(true)).is_err());
    execute(temp.path(), options(false)).unwrap();
    execute(temp.path(), options(false)).unwrap();
    execute(temp.path(), options(false)).unwrap();
    fs::write(temp.path().join("first"), b"same shared object").unwrap();
    assert_eq!(
        fs::read(temp.path().join("second")).unwrap(),
        b"same shared object"
    );
}

#[test]
fn suffix_is_relative_and_restore_ignores_header_flag() {
    let temp = TempDir::new().unwrap();
    put(temp.path(), "a", b"plain");
    let mut opt = options(false);
    opt.suffix = "../escape".into();
    assert!(execute(temp.path(), opt).is_err());
    assert!(!temp.path().join("DCDATA").exists());
    let mut opt = options(false);
    opt.suffix = ".bin".into();
    execute(temp.path(), opt).unwrap();
    assert_eq!(mapped_file(temp.path()).extension().unwrap(), "bin");
    let mut restore = options(true);
    restore.suffix = "ignored/while/restoring".into();
    execute(temp.path(), restore).unwrap();
    assert_eq!(fs::read(temp.path().join("a")).unwrap(), b"plain");
}

#[test]
#[cfg(unix)]
fn unix_names_permissions_and_nanosecond_timestamps_round_trip() {
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;
    for header in [false, true] {
        let temp = TempDir::new().unwrap();
        #[cfg(target_os = "linux")]
        let name = std::ffi::OsString::from_vec(b"native-\xff-name\\with:colon".to_vec());
        #[cfg(target_os = "macos")]
        let name = std::ffi::OsString::from("native-名字\\with:colon");
        let path = temp.path().join(name);
        let bytes = b"preserve native names, executable mode and every content byte";
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(
            std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789),
        )
        .unwrap();
        drop(file);
        let before = fs::metadata(&path).unwrap();
        execute(temp.path(), options(header)).unwrap();
        execute(temp.path(), options(false)).unwrap();
        let after = fs::metadata(&path).unwrap();
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(before.permissions().mode(), after.permissions().mode());
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
#[cfg(target_os = "linux")]
fn case_distinct_and_unreadable_files_map_without_reading_contents() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    put(temp.path(), "Name", b"upper");
    put(temp.path(), "name", b"lower");
    fs::set_permissions(temp.path().join("Name"), fs::Permissions::from_mode(0)).unwrap();
    execute(temp.path(), options(false)).unwrap();
    execute(temp.path(), options(false)).unwrap();
    assert_eq!(
        fs::metadata(temp.path().join("Name"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0
    );
    fs::set_permissions(temp.path().join("Name"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read(temp.path().join("Name")).unwrap(), b"upper");
    assert_eq!(fs::read(temp.path().join("name")).unwrap(), b"lower");
}

#[test]
#[cfg(unix)]
fn unix_dangling_and_relative_symlinks_round_trip() {
    let temp = TempDir::new().unwrap();
    put(temp.path(), "pkg/body", b"preserve the target");
    symlink_file("../missing", temp.path().join("pkg/dangling")).unwrap();
    symlink_file("body", temp.path().join("pkg/relative")).unwrap();
    execute(temp.path(), options(true)).unwrap();
    execute(temp.path(), options(false)).unwrap();
    assert_eq!(
        fs::read_link(temp.path().join("pkg/dangling")).unwrap(),
        Path::new("../missing")
    );
    assert_eq!(
        fs::read_link(temp.path().join("pkg/relative")).unwrap(),
        Path::new("body")
    );
    assert_eq!(
        fs::read(temp.path().join("pkg/relative")).unwrap(),
        b"preserve the target"
    );
}

#[test]
#[cfg(unix)]
fn special_files_fail_during_planning_without_blocking_or_changing_data() {
    use std::os::unix::ffi::OsStrExt;
    let temp = TempDir::new().unwrap();
    put(temp.path(), "ordinary", b"unchanged");
    let fifo = std::ffi::CString::new(temp.path().join("pipe").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let error = execute(temp.path(), options(true)).unwrap_err();
    assert!(format!("{error:#}").contains("Special files"));
    assert_eq!(
        fs::read(temp.path().join("ordinary")).unwrap(),
        b"unchanged"
    );
    execute(temp.path(), options(false)).unwrap();
    assert!(!temp.path().join("DCDATA").exists());
}
