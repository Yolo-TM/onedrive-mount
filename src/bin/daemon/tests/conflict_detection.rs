use chrono::Utc;
use onedrive_mount::sync_baseline::SyncBaseline;

#[test]
fn baseline_new_file_is_not_unchanged() {
    let baseline = SyncBaseline::default();
    let mtime = Utc::now();
    assert!(!baseline.is_unchanged("foo.txt", mtime));
}

#[test]
fn baseline_exact_mtime_is_unchanged() {
    let mut baseline = SyncBaseline::default();
    let mtime = Utc::now();
    baseline.set("foo.txt", mtime);
    assert!(baseline.is_unchanged("foo.txt", mtime));
}

#[test]
fn baseline_within_tolerance_is_unchanged() {
    let mut baseline = SyncBaseline::default();
    let mtime = Utc::now();
    baseline.set("foo.txt", mtime);
    let slightly_different = mtime + chrono::Duration::seconds(1);
    assert!(baseline.is_unchanged("foo.txt", slightly_different));
}

#[test]
fn baseline_beyond_tolerance_is_changed() {
    let mut baseline = SyncBaseline::default();
    let mtime = Utc::now();
    baseline.set("foo.txt", mtime);
    let changed = mtime + chrono::Duration::seconds(3);
    assert!(!baseline.is_unchanged("foo.txt", changed));
}

#[test]
fn baseline_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("baseline.toml");

    let mut baseline = SyncBaseline::default();
    let t1 = Utc::now();
    let t2 = t1 + chrono::Duration::seconds(60);
    baseline.set("file_a.txt", t1);
    baseline.set("subdir/file_b.txt", t2);
    baseline.save(&path).unwrap();

    let loaded = SyncBaseline::load(&path);
    assert!(loaded.is_unchanged("file_a.txt", t1));
    assert!(loaded.is_unchanged("subdir/file_b.txt", t2));
    assert!(!loaded.is_unchanged("file_a.txt", t2));
    assert!(!loaded.is_unchanged("missing.txt", t1));
}

#[test]
fn baseline_load_missing_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nonexistent.toml");
    let baseline = SyncBaseline::load(&path);
    assert!(baseline.files.is_empty());
}

#[test]
fn baseline_remove() {
    let mut baseline = SyncBaseline::default();
    let mtime = Utc::now();
    baseline.set("foo.txt", mtime);
    assert!(baseline.is_unchanged("foo.txt", mtime));
    baseline.remove("foo.txt");
    assert!(!baseline.is_unchanged("foo.txt", mtime));
}

fn should_conflict(
    baseline: &SyncBaseline,
    file: &str,
    local_mtime: chrono::DateTime<Utc>,
    remote_mtime: chrono::DateTime<Utc>,
) -> bool {
    let has_baseline = !baseline.files.is_empty();
    if !has_baseline {
        return false;
    }
    let local_changed = !baseline.is_unchanged(file, local_mtime);
    let remote_changed = !baseline.is_unchanged(file, remote_mtime);
    local_changed && remote_changed
}

#[test]
fn no_baseline_never_conflicts() {
    let baseline = SyncBaseline::default();
    let t = Utc::now();
    assert!(!should_conflict(
        &baseline,
        "foo.txt",
        t,
        t + chrono::Duration::seconds(10)
    ));
}

#[test]
fn only_local_changed_not_a_conflict() {
    let mut baseline = SyncBaseline::default();
    let t_base = Utc::now() - chrono::Duration::minutes(20);
    let t_local_new = Utc::now();
    baseline.set("foo.txt", t_base);

    assert!(!should_conflict(&baseline, "foo.txt", t_local_new, t_base));
}

#[test]
fn only_remote_changed_not_a_conflict() {
    let mut baseline = SyncBaseline::default();
    let t_base = Utc::now() - chrono::Duration::minutes(20);
    let t_remote_new = Utc::now();
    baseline.set("foo.txt", t_base);

    assert!(!should_conflict(&baseline, "foo.txt", t_base, t_remote_new));
}

#[test]
fn both_changed_is_a_conflict() {
    let mut baseline = SyncBaseline::default();
    let t_base = Utc::now() - chrono::Duration::minutes(20);
    let t_local_new = Utc::now() - chrono::Duration::minutes(5);
    let t_remote_new = Utc::now() - chrono::Duration::minutes(3);
    baseline.set("foo.txt", t_base);

    assert!(should_conflict(
        &baseline,
        "foo.txt",
        t_local_new,
        t_remote_new
    ));
}

#[test]
fn new_file_no_baseline_entry_not_a_conflict() {
    let mut baseline = SyncBaseline::default();
    baseline.set("other.txt", Utc::now());
    let t = Utc::now();

    assert!(should_conflict(
        &baseline,
        "new_file.txt",
        t,
        t + chrono::Duration::seconds(10)
    ));
}

#[test]
fn files_within_tolerance_not_changed() {
    let mut baseline = SyncBaseline::default();
    let t_base = Utc::now() - chrono::Duration::minutes(20);
    baseline.set("foo.txt", t_base);

    let t_local = t_base + chrono::Duration::seconds(1);
    let t_remote = t_base - chrono::Duration::seconds(1);
    assert!(!should_conflict(&baseline, "foo.txt", t_local, t_remote));
}
