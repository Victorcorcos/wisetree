use wisetree::cli::args::{parse_args, AppMode, CacheAction, CliCommand};

#[test]
fn no_args_defaults_to_menu_no_help_no_cli() {
    let p = parse_args(Vec::<String>::new()).unwrap();
    assert_eq!(p.mode, AppMode::Menu);
    assert!(!p.help);
    assert!(!p.version);
    assert!(p.cli_args.is_none());
    assert!(!p.is_from_wrapper);
}

#[test]
fn help_short_and_long_set_help_flag() {
    assert!(
        parse_args::<Vec<String>>(vec!["-h".to_string()])
            .unwrap()
            .help
    );
    assert!(
        parse_args::<Vec<String>>(vec!["--help".to_string()])
            .unwrap()
            .help
    );
}

#[test]
fn version_short_and_long() {
    assert!(
        parse_args::<Vec<String>>(vec!["-v".to_string()])
            .unwrap()
            .version
    );
    assert!(
        parse_args::<Vec<String>>(vec!["--version".to_string()])
            .unwrap()
            .version
    );
}

#[test]
fn from_wrapper_flag_detected() {
    let p = parse_args::<Vec<String>>(vec!["--from-wrapper".to_string()]).unwrap();
    assert!(p.is_from_wrapper);
    assert!(p.cli_args.is_none());
}

#[test]
fn mode_flag_sets_mode() {
    let p = parse_args::<Vec<String>>(vec!["--mode".into(), "create".into()]).unwrap();
    assert_eq!(p.mode, AppMode::Create);
}

#[test]
fn unknown_mode_falls_back_to_menu() {
    let p = parse_args::<Vec<String>>(vec!["-m".into(), "bogus".into()]).unwrap();
    assert_eq!(p.mode, AppMode::Menu);
}

#[test]
fn setup_is_not_a_valid_mode() {
    // Setup is reachable only via the menu, never via CLI.
    let p = parse_args::<Vec<String>>(vec!["setup".into()]).unwrap();
    assert_eq!(p.mode, AppMode::Menu);
    let p = parse_args::<Vec<String>>(vec!["--mode".into(), "setup".into()]).unwrap();
    assert_eq!(p.mode, AppMode::Menu);
}

#[test]
fn positional_command_resolves_mode() {
    let p = parse_args::<Vec<String>>(vec!["create".into()]).unwrap();
    assert_eq!(p.mode, AppMode::Create);
    // No flags → not non-interactive.
    assert!(p.cli_args.is_none());
}

#[test]
fn create_with_required_flags_is_non_interactive() {
    let p = parse_args::<Vec<String>>(vec![
        "create".into(),
        "-n".into(),
        "feat".into(),
        "-s".into(),
        "main".into(),
    ])
    .unwrap();
    let args = p.cli_args.expect("must be cli");
    assert_eq!(args.command, CliCommand::Create);
    assert_eq!(args.name.as_deref(), Some("feat"));
    assert_eq!(args.source.as_deref(), Some("main"));
}

#[test]
fn dashboard_json_flag_triggers_non_interactive() {
    let p = parse_args::<Vec<String>>(vec!["dashboard".into(), "--json".into()]).unwrap();
    let args = p.cli_args.expect("must be cli");
    assert_eq!(p.mode, AppMode::Dashboard);
    assert_eq!(args.command, CliCommand::Dashboard);
    assert!(args.json);
}

#[test]
fn dashboard_watch_flag_is_supported() {
    let p = parse_args::<Vec<String>>(vec!["dashboard".into(), "--watch".into()]).unwrap();
    let args = p.cli_args.expect("must be cli");
    assert_eq!(args.command, CliCommand::Dashboard);
    assert!(args.watch);
}

#[test]
fn mode_dashboard_is_supported() {
    let p = parse_args::<Vec<String>>(vec!["--mode".into(), "dashboard".into()]).unwrap();
    assert_eq!(p.mode, AppMode::Dashboard);
}

#[test]
fn unknown_long_flag_errors() {
    let err = parse_args::<Vec<String>>(vec!["--bogus".into()]).expect_err("must error");
    assert!(err.to_string().contains("Unknown option"));
}

#[test]
fn missing_value_after_flag_errors() {
    let err = parse_args::<Vec<String>>(vec!["-n".into()]).expect_err("missing value");
    assert!(err.to_string().contains("requires a value"));
}

#[test]
fn equal_sign_value_form_is_supported() {
    let p = parse_args::<Vec<String>>(vec![
        "create".into(),
        "--name=foo".into(),
        "--source=main".into(),
    ])
    .unwrap();
    let args = p.cli_args.expect("must be cli");
    assert_eq!(args.name.as_deref(), Some("foo"));
    assert_eq!(args.source.as_deref(), Some("main"));
}

#[test]
fn cache_command_without_action_opens_cache_mode() {
    let p = parse_args::<Vec<String>>(vec!["cache".into()]).unwrap();
    assert_eq!(p.mode, AppMode::Cache);
    assert!(p.cli_args.is_none());
}

#[test]
fn cache_subcommands_parse_as_non_interactive() {
    let p =
        parse_args::<Vec<String>>(vec!["cache".into(), "list".into(), "--json".into()]).unwrap();
    let args = p.cli_args.expect("must be cli");
    assert_eq!(
        args.command,
        CliCommand::Cache {
            action: CacheAction::List
        }
    );
    assert!(args.json);

    let p =
        parse_args::<Vec<String>>(vec!["cache".into(), "clear".into(), "--force".into()]).unwrap();
    let args = p.cli_args.expect("must be cli");
    assert_eq!(
        args.command,
        CliCommand::Cache {
            action: CacheAction::Clear
        }
    );
    assert!(args.force);
}
