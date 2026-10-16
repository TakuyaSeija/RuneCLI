use super::*;

#[test]
fn fresh_home_preparation_writes_no_help_which_comes_with_discovery() {
    let base = tests::temp("product-fresh");
    let store = base.join("store");
    let settings = base.join("settings");
    check(&store, &settings).unwrap();
    assert!(
        !store.exists() && !settings.exists(),
        "upgrade check is read-only"
    );
    prepare(&store, &settings, &[], &mut |_| {}).unwrap();
    assert_eq!(marked(&store).unwrap(), Some(VERSION));
    // The capability switch decides whether help is installed: a start alone
    // never does it.
    assert!(!settings.join("product-skills").exists());
    assert!(!settings.join("rune.toml").exists());
    assert!(!settings.join("master.key").exists());
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let (skills, _) = crate::capabilities::skills::discover(
        &crate::capabilities::config::Settings::default(),
        &settings,
        &project,
    )
    .unwrap();
    assert!(skills
        .iter()
        .any(|s| s.id == crate::capabilities::product::ID));
}

#[test]
fn format_19_upgrades_without_changing_user_choices_or_personal_skills() {
    let settings = tests::temp("product-v19");
    write_marker(&settings, 19).unwrap();
    let choices =
        "[ui]\ntheme = \"cyan\" # my choice\n[web_search]\nprovider = \"missing-choice\"\n";
    std::fs::write(settings.join("rune.toml"), choices).unwrap();
    std::fs::create_dir_all(settings.join("skills/mine")).unwrap();
    std::fs::write(settings.join("skills/mine/notes.md"), "mine").unwrap();
    prepare(&settings, &settings, &[], &mut |_| {}).unwrap();
    assert_eq!(
        std::fs::read_to_string(settings.join("rune.toml")).unwrap(),
        choices
    );
    assert_eq!(
        std::fs::read_to_string(settings.join("skills/mine/notes.md")).unwrap(),
        "mine"
    );
    assert_eq!(marked(&settings).unwrap(), Some(VERSION));
    let backups = std::fs::read_dir(settings.join("backups")).unwrap().count();
    prepare(&settings, &settings, &[], &mut |_| {}).unwrap();
    assert_eq!(
        std::fs::read_dir(settings.join("backups")).unwrap().count(),
        backups
    );
}

/// Owner, 2026-10-16: a changed official help file refused every start and
/// every upgrade check, so a live window never took a release over.
#[test]
fn a_damaged_official_help_copy_never_stops_a_start_an_upgrade_or_a_check() {
    let current = tests::temp("product-damaged-current");
    let package = crate::capabilities::product::ensure(&current).unwrap();
    std::fs::write(
        package
            .join(crate::capabilities::product::NAME)
            .join("SKILL.md"),
        "changed",
    )
    .unwrap();
    std::fs::write(
        package
            .join(crate::capabilities::product::NAME)
            .join(".SKILL.md.swp"),
        "swap",
    )
    .unwrap();
    check(&current, &current).unwrap();
    prepare(&current, &current, &[], &mut |_| {}).unwrap();
    let older = tests::temp("product-damaged-v19");
    write_marker(&older, 19).unwrap();
    std::fs::write(older.join("product-skills"), "not a folder").unwrap();
    check(&older, &older).unwrap();
    prepare(&older, &older, &[], &mut |_| {}).unwrap();
    assert_eq!(marked(&older).unwrap(), Some(VERSION));
}

/// A home whose upgrade stops before it changes anything: `backups` is a
/// file, so the step's backup folder cannot be made.
fn blocked_home(name: &str) -> PathBuf {
    let home = tests::temp(name);
    write_marker(&home, 19).unwrap();
    std::fs::write(home.join("backups"), "not a folder").unwrap();
    home
}

/// Owner, 2026-10-16: the goal of a release is that a person reaches it. A
/// failed upgrade leaves the home as it was, says so to every window of the
/// build before (which waits, longer after each failure, rather than each
/// failing at once), and is tried again the moment its cause is gone.
#[test]
fn a_failed_upgrade_holds_handovers_back_for_a_while_and_succeeds_once_its_cause_is_gone() {
    let home = blocked_home("refusal");
    check(&home, &home).unwrap();
    let error = prepare(&home, &home, &[], &mut |_| {}).unwrap_err();
    assert!(matches!(error, Error::Io(_)), "{error:?}");
    assert_eq!(state(&home, &home).unwrap(), State::Older(19));
    let refusal = Refusal::read(&home).expect("the failure is noted in the home");
    assert_eq!(refusal.build, crate::VERSION);
    assert_eq!(refusal.attempts, 1);
    assert!(refusal.stands());
    let held = check(&home, &home).unwrap_err().to_string();
    assert!(held.contains("tried again"), "{held}");
    prepare(&home, &home, &[], &mut |_| {}).unwrap_err();
    let second = Refusal::read(&home).unwrap();
    assert_eq!(second.attempts, 2);
    assert!(second.wait() > std::time::Duration::from_secs(RETRY_AFTER[0]));
    // Its wait over, the build is asked again: still blocked, still noted.
    let mut over = second;
    over.at -= 2 * 3600 * 1000;
    std::fs::write(home.join(REFUSED), serde_json::to_vec(&over).unwrap()).unwrap();
    check(&home, &home).unwrap();
    // The cause gone, the upgrade succeeds and the note goes with it.
    std::fs::remove_file(home.join("backups")).unwrap();
    prepare(&home, &home, &[], &mut |_| {}).unwrap();
    assert_eq!(marked(&home).unwrap(), Some(VERSION));
    assert!(Refusal::read(&home).is_none());
    check(&home, &home).unwrap();
}

#[test]
fn a_refusal_of_another_build_holds_nothing_back() {
    let home = tests::temp("refusal-other-build");
    write_marker(&home, 19).unwrap();
    let refusal = Refusal {
        build: "0.0.1".into(),
        attempts: 3,
        at: crate::store::now_ms(),
        reason: "an older build's trouble".into(),
    };
    std::fs::create_dir_all(home.join("run")).unwrap();
    std::fs::write(home.join(REFUSED), serde_json::to_vec(&refusal).unwrap()).unwrap();
    check(&home, &home).unwrap();
}

/// Nothing is touched when the upgrade could not finish anyway: no backup
/// folder, no marker change, the cause named as what it is.
#[test]
fn an_upgrade_that_cannot_write_stops_before_changing_anything() {
    let home = tests::temp("preflight");
    write_marker(&home, 19).unwrap();
    std::fs::create_dir_all(home.join("run")).unwrap();
    let settings = home.join("settings");
    std::fs::create_dir(&settings).unwrap();
    std::fs::write(settings.join("rune.toml"), "[ui]\ntheme = \"cyan\"\n").unwrap();
    let read_only = crate::platform::fixture::Restricted::new(&settings, 0o500).unwrap();
    let result = prepare(&home, &settings, &[], &mut |_| {});
    drop(read_only);
    let error = result.unwrap_err();
    assert!(
        matches!(&error, Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied),
        "{error:?}"
    );
    assert!(!home.join("backups").exists());
    assert_eq!(marked(&home).unwrap(), Some(19));
}

/// A step that fails part way keeps what went wrong as what it was — a full
/// disk is still a full disk — so the person is told what to do.
#[test]
fn a_step_that_fails_part_way_keeps_its_cause() {
    let home = tests::temp("step-io-kind");
    write_marker(&home, 18).unwrap();
    let config = home.join("rune.toml");
    std::fs::write(&config, "[ui]\ntheme = \"cyan\"\n").unwrap();
    let unreadable = crate::platform::fixture::Restricted::new(&config, 0o000).unwrap();
    let result = prepare(&home, &home, &[], &mut |_| {});
    drop(unreadable);
    let error = result.unwrap_err();
    assert!(error.to_string().contains("undone"), "{error}");
    assert!(
        matches!(&error, Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied),
        "{error:?}"
    );
    assert_eq!(marked(&home).unwrap(), Some(18));
}
