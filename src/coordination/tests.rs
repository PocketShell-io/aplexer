//! Unit tests for coordination: lock-serialized concurrent join/leave, the
//! stale-claim verdict, alias/worktree canonicalization and relations,
//! conservative overlaps, and UUID-addressed mailbox history surviving both
//! acknowledgement and session movement.

use super::state;
use super::*;
use crate::messaging::{
    ack_messages, ensure_workspace, now_secs, write_message, Delivery, MessageEnvelope,
    MessageFrom, Recipient, MESSAGE_SCHEMA_VERSION,
};
use crate::{atomic_write_json, canonical_workspace, Phase, SessionRecord};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

struct Isolated {
    _runtime: TempDir,
    _state: TempDir,
    paths: Paths,
}

fn isolated() -> Isolated {
    let runtime = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let config = runtime.path().join("config.toml");
    let paths = Paths::discover_with(
        Some(runtime.path()),
        Some(state.path()),
        Some(config.as_path()),
    )
    .unwrap();
    Isolated {
        _runtime: runtime,
        _state: state,
        paths,
    }
}

/// Persists a fixture record and returns its id. `live` pins the worker pid
/// to this process so the observed state is `running`; otherwise the zero
/// clock plus a dead worker read as `broken`/`exited`.
fn record_at(
    paths: &Paths,
    workspace: &std::path::Path,
    tag: &str,
    phase: Phase,
    live: bool,
) -> Uuid {
    let mut record = SessionRecord::fixture(workspace, tag);
    record.phase = phase;
    if live {
        record.worker_pid = Some(std::process::id());
    }
    // `read_session_record` validates the socket path against the same
    // Paths, so the fixture's placeholder would fail every load.
    record.socket_path = paths.socket(record.id);
    record.history_path = paths.history(record.id);
    atomic_write_json(&paths.record(record.id), &record).unwrap();
    record.id
}

fn join_simple(paths: &Paths, id: Uuid, workspace: &std::path::Path, task: &str) -> Participation {
    join(paths, id, workspace, task, WorkMode::Edit, &[]).unwrap()
}

#[test]
fn join_requires_a_session_a_task_and_contained_scopes() {
    let isolated = isolated();
    let workspace = TempDir::new().unwrap();
    let ghost = Uuid::new_v4();
    assert!(
        join(
            &isolated.paths,
            ghost,
            workspace.path(),
            "task",
            WorkMode::Edit,
            &[]
        )
        .is_err(),
        "a declaration without a session record must be rejected"
    );

    let id = record_at(
        &isolated.paths,
        workspace.path(),
        "author",
        Phase::Running,
        true,
    );
    assert!(join(
        &isolated.paths,
        id,
        workspace.path(),
        "   ",
        WorkMode::Edit,
        &[]
    )
    .is_err());
    assert!(join(
        &isolated.paths,
        id,
        workspace.path(),
        "",
        WorkMode::Edit,
        &[]
    )
    .is_err());
    let file_workspace = workspace.path().join("plain.txt");
    std::fs::write(&file_workspace, b"not a directory").unwrap();
    assert!(
        join(
            &isolated.paths,
            id,
            &file_workspace,
            "task",
            WorkMode::Edit,
            &[]
        )
        .is_err(),
        "an existing file is not a valid workspace"
    );
    assert!(context(&isolated.paths, None, &file_workspace).is_err());
    for scope in ["../outside", "/etc", "a/../..", "src/\u{1b}[31m"] {
        assert!(
            join(
                &isolated.paths,
                id,
                workspace.path(),
                "task",
                WorkMode::Edit,
                &[scope.to_string()]
            )
            .is_err(),
            "scope {scope:?} must be rejected"
        );
    }
    // Nested-but-contained and glob scopes are fine.
    assert!(join(
        &isolated.paths,
        id,
        workspace.path(),
        "task",
        WorkMode::Edit,
        &["./src/**/*.rs".to_string(), "src".to_string()]
    )
    .is_ok());
}

#[test]
fn concurrent_joins_upsert_and_leaves_serialize() {
    let isolated = isolated();
    let workspace = TempDir::new().unwrap();
    let id = record_at(
        &isolated.paths,
        workspace.path(),
        "concurrent",
        Phase::Running,
        true,
    );

    let ws = workspace.path();
    let results: Vec<_> = std::thread::scope(|scope| {
        (0..8)
            .map(|round| {
                let paths = &isolated.paths;
                scope.spawn(move || {
                    join(
                        paths,
                        id,
                        ws,
                        &format!("task {round}"),
                        WorkMode::Edit,
                        &[format!("dir-{round}/**")],
                    )
                    .unwrap()
                })
            })
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    assert_eq!(results.len(), 8);

    let claims = state::load_tolerant(&isolated.paths, id).unwrap();
    assert_eq!(
        claims.len(),
        1,
        "joins to one workspace upsert, never duplicate"
    );
    assert_eq!(claims[0].workspace, canonical_workspace(ws).unwrap());

    let released = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let paths = &isolated.paths;
            let released = Arc::clone(&released);
            scope.spawn(move || {
                if leave(paths, id, ws).unwrap() {
                    released.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    assert_eq!(
        released.load(Ordering::SeqCst),
        1,
        "exactly one concurrent leave releases; the rest see nothing to release"
    );
    assert!(state::load_tolerant(&isolated.paths, id)
        .unwrap()
        .is_empty());
}

#[test]
fn idle_and_exit_never_release_but_claims_go_stale() {
    let isolated = isolated();
    let workspace = TempDir::new().unwrap();
    let live = record_at(
        &isolated.paths,
        workspace.path(),
        "live",
        Phase::Running,
        true,
    );
    let gone = record_at(
        &isolated.paths,
        workspace.path(),
        "gone",
        Phase::Exited,
        false,
    );
    let crashed = record_at(
        &isolated.paths,
        workspace.path(),
        "crashed",
        Phase::Running,
        false,
    );

    join_simple(&isolated.paths, live, workspace.path(), "still working");
    join_simple(&isolated.paths, gone, workspace.path(), "left this behind");
    join_simple(&isolated.paths, crashed, workspace.path(), "worker died");

    let context = context(&isolated.paths, None, workspace.path()).unwrap();
    let by_tag = |tag: &str| {
        context
            .peers
            .iter()
            .find(|p| p.session.tag == tag)
            .and_then(|p| p.declarations.first())
            .unwrap_or_else(|| panic!("no declaration for {tag}"))
    };
    assert!(!by_tag("live").stale);
    assert!(by_tag("gone").stale, "an exited session's claim is stale");
    assert!(
        by_tag("crashed").stale,
        "a dead worker's claim is stale (observed broken)"
    );
    // All three remain visible: nothing reaped, nothing released by idleness.
    assert_eq!(context.peers.len(), 3);
}

#[test]
#[cfg(unix)]
fn aliases_collapse_to_one_claim_and_relation_uses_checkout_facts() {
    let isolated = isolated();
    let real = TempDir::new().unwrap();
    let alias = TempDir::new().unwrap();
    let alias_path = alias.path().join("link");
    std::os::unix::fs::symlink(real.path(), &alias_path).unwrap();

    let id = record_at(
        &isolated.paths,
        real.path(),
        "aliased",
        Phase::Running,
        true,
    );
    let via_alias = join(
        &isolated.paths,
        id,
        &alias_path,
        "via alias",
        WorkMode::Edit,
        &["src/**".to_string()],
    )
    .unwrap();
    assert_eq!(
        via_alias.workspace,
        canonical_workspace(real.path()).unwrap()
    );

    // The same directory spelled differently is the same claim.
    let again = join(
        &isolated.paths,
        id,
        real.path(),
        "via real",
        WorkMode::Read,
        &[],
    )
    .unwrap();
    assert_eq!(again.workspace, via_alias.workspace);
    let claims = state::load_tolerant(&isolated.paths, id).unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(
        claims[0].mode,
        WorkMode::Read,
        "the re-join replaced the claim"
    );

    assert!(
        leave(&isolated.paths, id, &alias_path).unwrap(),
        "release works through an alias"
    );
    assert!(!leave(&isolated.paths, id, real.path()).unwrap());

    // Worktree relations: a linked worktree is a different checkout of the
    // same repository; two sessions in the same directory share a checkout.
    let repo = init_repo(real.path().join("repo"));
    let worktree = repo.parent().unwrap().join("wt");
    run(&format!(
        "git -C {} worktree add -b side {}",
        repo.display(),
        worktree.display()
    ));

    let _main_session = record_at(&isolated.paths, &repo, "main-tree", Phase::Running, true);
    let wt_session = record_at(&isolated.paths, &worktree, "wt-tree", Phase::Running, true);
    join_simple(
        &isolated.paths,
        wt_session,
        &repo,
        "declaring the main tree",
    );

    // A session whose origin is a plain subdirectory of the checkout is the
    // same checkout as the root, without any declaration.
    let subdirectory = repo.join("src");
    std::fs::create_dir(&subdirectory).unwrap();
    let _sub_session = record_at(
        &isolated.paths,
        &subdirectory,
        "subdir",
        Phase::Running,
        true,
    );
    // A sibling-worktree session that never declared anything is related
    // by repository only.
    let _wt_bare = record_at(&isolated.paths, &worktree, "wt-bare", Phase::Running, true);

    let context_root = context(&isolated.paths, None, &repo).unwrap();
    let relation = |tag: &str| {
        context_root
            .peers
            .iter()
            .find(|p| p.session.tag == tag)
            .unwrap_or_else(|| panic!("no peer {tag}"))
            .relation
    };
    assert_eq!(relation("main-tree"), Relation::SameCheckout);
    // wt-tree declared the main tree, so its best relation is by target.
    assert_eq!(relation("wt-tree"), Relation::SameCheckout);
    assert_eq!(relation("subdir"), Relation::SameCheckout);
    assert_eq!(relation("wt-bare"), Relation::RelatedWorktree);
    // The subdirectory view includes the checkout root's session too.
    let from_subdir = context(&isolated.paths, None, &subdirectory).unwrap();
    assert!(from_subdir
        .peers
        .iter()
        .any(|p| p.session.tag == "main-tree" && p.relation == Relation::SameCheckout));
    assert!(from_subdir
        .peers
        .iter()
        .any(|p| p.session.tag == "wt-bare" && p.relation == Relation::RelatedWorktree));

    // The declaration itself records the git facts of the declared directory.
    let claims = state::load_tolerant(&isolated.paths, wt_session).unwrap();
    let git = claims[0].git.as_ref().unwrap();
    assert_eq!(git.worktree, canonical_workspace(&repo).unwrap());
    assert_eq!(
        git.common_dir,
        canonical_workspace(&repo.join(".git")).unwrap()
    );
}

#[cfg_attr(windows, allow(dead_code))]
fn run(command: &str) {
    let output = Command::new("bash")
        .arg("-c")
        .arg(command)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{command} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg_attr(windows, allow(dead_code))]
fn init_repo(path: std::path::PathBuf) -> std::path::PathBuf {
    run(&format!(
        "git init -q {} && git -C {} -c user.email=t@example.com -c user.name=t commit --allow-empty -q -m init",
        path.display(),
        path.display()
    ));
    path
}

#[test]
fn overlaps_are_reported_only_for_shared_directories() {
    let isolated = isolated();
    let shared = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();

    let me = record_at(&isolated.paths, shared.path(), "me", Phase::Running, true);
    let editor = record_at(
        &isolated.paths,
        shared.path(),
        "editor",
        Phase::Running,
        true,
    );
    let reader = record_at(
        &isolated.paths,
        shared.path(),
        "reader",
        Phase::Running,
        true,
    );
    let stranger = record_at(
        &isolated.paths,
        elsewhere.path(),
        "stranger",
        Phase::Running,
        true,
    );

    join(
        &isolated.paths,
        me,
        shared.path(),
        "my work",
        WorkMode::Edit,
        &["src/core/**".to_string()],
    )
    .unwrap();
    join(
        &isolated.paths,
        editor,
        shared.path(),
        "their work",
        WorkMode::Edit,
        &["src/core/file.rs".to_string()],
    )
    .unwrap();
    join(
        &isolated.paths,
        reader,
        shared.path(),
        "a review",
        WorkMode::Review,
        &[],
    )
    .unwrap();
    join(
        &isolated.paths,
        stranger,
        elsewhere.path(),
        "unrelated",
        WorkMode::Edit,
        &[],
    )
    .unwrap();
    let disjoint = record_at(
        &isolated.paths,
        shared.path(),
        "disjoint",
        Phase::Running,
        true,
    );
    join(
        &isolated.paths,
        disjoint,
        shared.path(),
        "parallel work",
        WorkMode::Edit,
        &["src/hooks/**".to_string()],
    )
    .unwrap();

    let context_me = context(&isolated.paths, Some(me), shared.path()).unwrap();
    let exclusive: Vec<_> = context_me
        .overlaps
        .iter()
        .filter(|o| o.exclusive)
        .map(|o| o.peer_tag.as_str())
        .collect();
    assert_eq!(exclusive, ["editor"], "only edit/edit overlap is exclusive");
    assert!(
        !context_me.overlaps.iter().any(|o| o.peer_tag == "disjoint"),
        "provably disjoint edit scopes share no physical path: {context_me:?}"
    );
    let reader_overlap = context_me
        .overlaps
        .iter()
        .find(|o| o.peer_tag == "reader")
        .unwrap();
    assert!(!reader_overlap.exclusive);
    assert_eq!(reader_overlap.your_mode, Some(WorkMode::Edit));

    // A peer declaring the directory I physically live in counts as shared
    // even when I hold no claim there.
    let bare = record_at(&isolated.paths, shared.path(), "bare", Phase::Running, true);
    join(
        &isolated.paths,
        stranger,
        shared.path(),
        "walking in",
        WorkMode::Edit,
        &[],
    )
    .unwrap();
    let context_bare = context(&isolated.paths, Some(bare), shared.path()).unwrap();
    let walking_in = context_bare
        .overlaps
        .iter()
        .find(|o| o.peer_tag == "stranger")
        .unwrap();
    assert_eq!(walking_in.your_mode, None);
    assert!(!walking_in.exclusive);

    let rendered = render_context(&context_bare);
    assert!(rendered.contains("not instructions"), "{rendered}");
    assert!(rendered.contains("editor"), "{rendered}");
    assert!(rendered.contains("advisory"), "{rendered}");
}

#[test]
fn retained_uuid_history_survives_ack_and_excludes_unrelated_mail() {
    let isolated = isolated();
    let home = TempDir::new().unwrap();
    let foreign = TempDir::new().unwrap();

    let me = record_at(
        &isolated.paths,
        home.path(),
        "traveler",
        Phase::Running,
        true,
    );
    let someone = Uuid::new_v4();

    ensure_workspace(&isolated.paths, foreign.path()).unwrap();
    let addressed = envelope(
        foreign.path(),
        Recipient::Tag {
            tag: "traveler".to_string(),
            session_id: Some(me),
        },
    );
    let broadcast = envelope(foreign.path(), Recipient::Broadcast { broadcast: true });
    let tag_only = envelope(
        foreign.path(),
        Recipient::Tag {
            tag: "traveler".to_string(),
            session_id: None,
        },
    );
    for message in [&addressed, &broadcast, &tag_only] {
        write_message(&isolated.paths, message).unwrap();
    }

    // The foreign mailbox is discovered because one message names this
    // session's UUID explicitly — broadcast and tag-only traffic in the
    // same retained mailbox stay out.
    let workspaces = mailbox_workspaces(&isolated.paths, me).unwrap();
    assert!(workspaces.contains(&canonical_workspace(home.path()).unwrap()));
    assert!(workspaces.contains(&canonical_workspace(foreign.path()).unwrap()));

    let unread = unread_messages(&isolated.paths, me).unwrap();
    assert_eq!(
        unread.iter().map(|m| m.id).collect::<Vec<_>>(),
        [addressed.id],
        "only the explicitly addressed message is mine"
    );
    // A different session, even with the same tag, gets nothing from that
    // mailbox: no broadcasts, no tag matches, only exact UUIDs.
    assert!(unread_messages(&isolated.paths, someone)
        .unwrap()
        .is_empty());

    let rendered = render_context(&context(&isolated.paths, Some(me), home.path()).unwrap());
    assert!(rendered.contains(&addressed.id.to_string()), "{rendered}");
    assert!(
        !rendered.contains(&addressed.body),
        "context never carries bodies"
    );

    // Acknowledged history keeps the mailbox attached for show/reply, but
    // stops being unread.
    ack_messages(&isolated.paths, foreign.path(), me, &[addressed.id]).unwrap();
    assert!(unread_messages(&isolated.paths, me).unwrap().is_empty());
    assert!(mailbox_workspaces(&isolated.paths, me)
        .unwrap()
        .contains(&canonical_workspace(foreign.path()).unwrap()));

    // A declaration subscribes the declared workspace to ordinary inbox
    // traffic: broadcasts there become unread for me.
    let declared = TempDir::new().unwrap();
    join_simple(&isolated.paths, me, declared.path(), "working here");
    let subscription_broadcast =
        envelope(declared.path(), Recipient::Broadcast { broadcast: true });
    write_message(&isolated.paths, &subscription_broadcast).unwrap();
    let unread = unread_messages(&isolated.paths, me).unwrap();
    assert_eq!(
        unread.iter().map(|m| m.id).collect::<Vec<_>>(),
        [subscription_broadcast.id]
    );
    assert!(unread_messages(&isolated.paths, someone)
        .unwrap()
        .is_empty());
}

/// The sidecar's serialized next state is checked against the byte cap
/// BEFORE writing: a rejected push must leave the previous sidecar exactly
/// intact, so the session's own claims stay readable and releasable.
#[test]
fn oversized_state_is_rejected_preserving_the_previous_sidecar() {
    let isolated = isolated();
    let workspace = TempDir::new().unwrap();
    let id = record_at(
        &isolated.paths,
        workspace.path(),
        "sized",
        Phase::Running,
        true,
    );
    let big_scope = "x".repeat(500);
    let scopes = vec![big_scope; 64];
    let mut accepted = 0;
    for index in 0..12 {
        let target = workspace.path().join(format!("dir-{index}"));
        let pushed = state::update(&isolated.paths, id, |claims| {
            claims.push(Participation {
                workspace: canonical_workspace(&target).unwrap(),
                task: "overflow probe".to_string(),
                mode: WorkMode::Edit,
                scopes: scopes.clone(),
                git: None,
                created_at_ms: 0,
                updated_at_ms: 0,
            });
            Ok(((), true))
        });
        if pushed.is_ok() {
            accepted += 1;
        }
    }
    assert!(accepted < 12, "the byte cap must reject some pushes");
    let claims = state::load_tolerant(&isolated.paths, id).unwrap();
    assert_eq!(
        claims.len(),
        accepted,
        "a rejected push must not leave partial state behind"
    );
}

fn envelope(workspace: &std::path::Path, to: Recipient) -> MessageEnvelope {
    MessageEnvelope {
        schema_version: MESSAGE_SCHEMA_VERSION,
        id: uuid::Uuid::now_v7(),
        workspace: workspace.to_path_buf(),
        created_at: now_secs(),
        from: MessageFrom {
            session_id: Some(Uuid::new_v4()),
            workspace: None,
            tag: Some("peer".to_string()),
            engine: Some("shell".to_string()),
            profile: None,
            external: false,
        },
        to,
        kind: "note".to_string(),
        reply_to: None,
        body: "please look at the failing test".to_string(),
        data: None,
        delivery: Delivery::default(),
    }
}

/// A session whose record is gone keeps its UUID-addressed mail findable:
/// the mailbox is discovered and the envelope is returned whole.
#[test]
fn unread_and_mailbox_listing_tolerate_a_missing_record() {
    let isolated = isolated();
    let ghost = Uuid::new_v4();
    let home = TempDir::new().unwrap();
    ensure_workspace(&isolated.paths, home.path()).unwrap();
    let message = envelope(
        home.path(),
        Recipient::Tag {
            tag: "ghost".to_string(),
            session_id: Some(ghost),
        },
    );
    write_message(&isolated.paths, &message).unwrap();

    let workspaces = mailbox_workspaces(&isolated.paths, ghost).unwrap();
    assert_eq!(workspaces, [canonical_workspace(home.path()).unwrap()]);
    let unread = unread_messages(&isolated.paths, ghost).unwrap();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].id, message.id);
}

/// The snapshot never blocks or errors on a busy mailbox: that pass skips
/// it, and once the lock is released the next pass answers normally. The
/// snapshot's two halves stay consistent with the standalone wrappers,
/// which are the same single pass.
#[test]
fn snapshot_skips_a_busy_mailbox_and_matches_the_wrappers() {
    use crate::messaging::mailbox_lock_path;
    let isolated = isolated();
    let home = TempDir::new().unwrap();
    let foreign = TempDir::new().unwrap();
    let me = record_at(
        &isolated.paths,
        home.path(),
        "skipper",
        Phase::Running,
        true,
    );
    ensure_workspace(&isolated.paths, foreign.path()).unwrap();
    let addressed = envelope(
        foreign.path(),
        Recipient::Tag {
            tag: "skipper".to_string(),
            session_id: Some(me),
        },
    );
    write_message(&isolated.paths, &addressed).unwrap();
    let foreign_canonical = canonical_workspace(foreign.path()).unwrap();

    let mp = crate::messaging::message_paths(&isolated.paths, &foreign_canonical);
    let held = crate::FileLock::exclusive(&mailbox_lock_path(&mp), true).unwrap();
    let snapshot = inbox_snapshot(&isolated.paths, me).unwrap();
    assert!(!snapshot.mailboxes.contains(&foreign_canonical));
    assert!(snapshot.unread.is_empty());
    drop(held);

    let snapshot = inbox_snapshot(&isolated.paths, me).unwrap();
    assert_eq!(
        snapshot.unread.iter().map(|m| m.id).collect::<Vec<_>>(),
        [addressed.id]
    );
    assert_eq!(
        mailbox_workspaces(&isolated.paths, me).unwrap(),
        snapshot.mailboxes
    );
}
