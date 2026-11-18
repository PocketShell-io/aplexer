// -- `a -<tag>`, tmuxctl's dash-suffix create-or-attach idiom, and the
// argv rewrite in commands.rs that smuggles it past clap (`quick-launch
// --tag <tag>`) --

#[test]
fn dash_suffix_rewrites_to_quick_launch_with_an_explicit_tag() {
    assert_eq!(
        rewrite_quick_attach_args(vec!["a".into(), "-review".into()]),
        vec!["a", "quick-launch", "--tag", "review"]
    );
    // Words after the dash-tag still pick engine/command.
    assert_eq!(
        rewrite_quick_attach_args(vec!["a".into(), "-review".into(), "claude".into()]),
        vec!["a", "quick-launch", "--tag", "review", "claude"]
    );
}

#[test]
fn rewritten_dash_suffix_parses_as_quick_launch_with_tag() {
    let rewritten = rewrite_quick_attach_args(vec!["a".into(), "-review".into(), "claude".into()]);
    match Cli::try_parse_from(rewritten).unwrap().command {
        Some(Commands::QuickLaunch(quick)) => {
            assert_eq!(quick.tag.as_deref(), Some("review"));
            assert_eq!(quick.rest, vec!["claude".to_string()]);
        }
        _ => panic!("expected quick-launch"),
    }
}

#[test]
fn bare_dash_and_quick_index_rewrites_are_unchanged() {
    assert_eq!(
        rewrite_quick_attach_args(vec!["a".into(), "-".into(), "claude".into()]),
        vec!["a", "quick-launch", "claude"]
    );
    assert_eq!(
        rewrite_quick_attach_args(vec!["a".into(), "2".into(), "review".into()]),
        vec!["a", "quick-attach", "2", "review"]
    );
    // No rewrite at all without a dash or index first argument.
    assert_eq!(
        rewrite_quick_attach_args(vec!["a".into(), "list".into()]),
        vec!["a", "list"]
    );
}

#[test]
fn real_flags_are_left_for_clap_not_taken_as_tags() {
    for argv in [["-h"], ["-V"], ["--json"]] {
        let mut args = vec!["a".to_string()];
        args.extend(argv.iter().map(|s| s.to_string()));
        assert_eq!(rewrite_quick_attach_args(args.clone()), args, "argv {argv:?}");
    }
}

#[test]
fn configured_shortcut_rewrites_to_quick_shortcut_and_parses() {
    let mut shortcuts = BTreeMap::new();
    shortcuts.insert(
        "dev".to_string(),
        ShortcutConfig {
            workspace: PathBuf::from("/srv/bot"),
            tag: "mgmt".to_string(),
        },
    );

    let rewritten = rewrite_shortcut_args(vec!["a".into(), "dev".into()], &shortcuts);
    assert_eq!(rewritten, vec!["a", "quick-shortcut", "dev"]);
    match Cli::try_parse_from(rewritten).unwrap().command {
        Some(Commands::QuickShortcut(args)) => assert_eq!(args.name, "dev"),
        _ => panic!("expected quick-shortcut"),
    }
}

#[test]
fn subcommands_aliases_numbers_and_flags_take_precedence_over_shortcuts() {
    let mut shortcuts = BTreeMap::new();
    for reserved in [
        "list", "ls", "start", "attach", "open", "shortcut", "help", "123", "-h", "--json",
    ] {
        shortcuts.insert(
            reserved.to_string(),
            ShortcutConfig {
                workspace: PathBuf::from("/srv/bot"),
                tag: "main".to_string(),
            },
        );
    }

    for word in [
        "list", "ls", "start", "attach", "open", "shortcut", "help", "123", "-h", "--json",
        "unknown",
    ] {
        let args = vec!["a".to_string(), word.to_string()];
        assert_eq!(
            rewrite_shortcut_args(args.clone(), &shortcuts),
            args,
            "word {word:?} must not be rewritten as shortcut"
        );
    }
}

#[test]
fn shortcut_name_validation_rejects_reserved_and_ambiguous_names() {
    assert!(validate_shortcut_name("dev").is_ok());
    assert!(validate_shortcut_name("tofu").is_ok());
    assert!(validate_shortcut_name("orin-1_a").is_ok());

    for bad in ["", "-dev", "42", "list", "ls", "ps", "help", "shortcut", "a b"] {
        assert!(
            validate_shortcut_name(bad).is_err(),
            "expected {bad:?} to be rejected"
        );
    }
}
