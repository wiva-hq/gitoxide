use std::{path::Path, sync::atomic::AtomicBool};

use gix::remote::{
    Direction,
    push::{self, Lease, Rejection, Status},
};
use gix_testtools::{Result, TestResult, git, tempfile::TempDir};

/// Create the client and its servers, see `make_remote_push_repos.sh`.
fn fixture() -> Result<TempDir> {
    gix_testtools::scripted_fixture_writable("make_remote_push_repos.sh")
}

/// Open the client with `remote.origin.url` set to `url`, along with `config` overrides.
fn client(dir: &Path, url: &str, config: &[&str]) -> Result<gix::Repository> {
    let overrides = ["user.name=gitoxide", "user.email=gitoxide@localhost"]
        .into_iter()
        .map(ToOwned::to_owned)
        .chain(Some(format!("remote.origin.url={url}")))
        .chain(config.iter().map(|value| (*value).to_owned()));
    Ok(gix::open_opts(
        dir.join("client"),
        gix::open::Options::isolated().config_overrides(overrides),
    )?)
}

fn local_server(dir: &Path, name: &str) -> String {
    gix::path::realpath(dir.join(name))
        .expect("the server exists")
        .to_string_lossy()
        .into_owned()
}

fn options(refspecs: &[&str]) -> Result<push::Options> {
    Ok(push::Options {
        refspecs: refspecs
            .iter()
            .map(|spec| -> Result<gix::refspec::RefSpec> {
                Ok(gix::refspec::parse((*spec).into(), gix::refspec::parse::Operation::Push)?.to_owned())
            })
            .collect::<Result<_>>()?,
        ..Default::default()
    })
}

fn push(repo: &gix::Repository, options: push::Options) -> Result<push::Outcome> {
    Ok(repo
        .find_remote("origin")?
        .connect(Direction::Push)?
        .prepare_push(gix::progress::Discard, options)?
        .send(gix::progress::Discard, &AtomicBool::default())?)
}

fn statuses(outcome: &push::Outcome) -> Vec<(String, Status)> {
    outcome
        .updates
        .iter()
        .map(|update| (update.remote_ref.as_bstr().to_string(), update.status.clone()))
        .collect()
}

fn rev_parse(repo_dir: &Path, rev: &str) -> Result<String> {
    Ok(git(repo_dir, &format!("rev-parse --verify {rev}"))?.trim().to_owned())
}

fn refs(repo_dir: &Path) -> Result<String> {
    git(repo_dir, "for-each-ref --format=%(refname)")
}

/// Assert that `git fsck` is happy with `server`, and that the history of `remote_ref` is the one of `local_rev` in the client.
fn assert_server_has(dir: &Path, server: &str, remote_ref: &str, local_rev: &str) -> TestResult {
    let server = &dir.join(server);
    git(server, "fsck")?;
    assert_eq!(
        git(server, &format!("log --format=%H {remote_ref}"))?,
        git(dir.join("client"), &format!("log --format=%H {local_rev}"))?,
        "the server has the complete history of {local_rev} in {remote_ref}"
    );
    Ok(())
}

#[test]
fn create_fast_forward_and_tag_then_nothing_is_left_to_do() -> TestResult {
    let dir = fixture()?;
    let repo = client(dir.path(), &local_server(dir.path(), "server.git"), &[])?;
    let main_id = repo.rev_parse_single("main")?.detach();
    let opts = options(&["main", "main:created", "v2"])?;

    let outcome = push(&repo, opts.clone())?;
    assert_eq!(
        statuses(&outcome),
        [
            ("refs/heads/main".to_owned(), Status::Ok),
            ("refs/heads/created".to_owned(), Status::Ok),
            ("refs/tags/v2".to_owned(), Status::Ok),
        ],
        "abbreviated destinations take the category of their source if the remote doesn't have them"
    );
    let created = &outcome.updates[1];
    assert_eq!(created.remote_id, None, "the branch didn't exist");
    assert_eq!(
        created.local_ref.as_ref().map(|r| r.as_bstr().to_string()),
        Some("refs/heads/main".to_owned())
    );
    assert_server_has(dir.path(), "server.git", "main", "main")?;
    assert_server_has(dir.path(), "server.git", "created", "main")?;
    assert_eq!(rev_parse(&dir.path().join("server.git"), "v2")?, main_id.to_string());
    for tracking_ref in ["refs/remotes/origin/main", "refs/remotes/origin/created"] {
        assert_eq!(
            repo.find_reference(tracking_ref)?.id(),
            main_id,
            "remote-tracking branches follow what the remote accepted"
        );
    }
    assert_eq!(outcome.updates[2].tracking_ref, None, "tags aren't tracked");

    let outcome = push(&repo, opts)?;
    assert!(
        outcome.updates.iter().all(|u| u.status == Status::UpToDate),
        "pushing again has no effect: {:?}",
        statuses(&outcome)
    );
    Ok(())
}

#[test]
fn non_fast_forwards_need_force() -> TestResult {
    let dir = fixture()?;
    let repo = client(dir.path(), &local_server(dir.path(), "server.git"), &[])?;
    let server = dir.path().join("server.git");
    let server_main = rev_parse(&server, "main")?;

    let outcome = push(&repo, options(&["rewritten:main"])?)?;
    assert_eq!(
        statuses(&outcome),
        [(
            "refs/heads/main".to_owned(),
            Status::Rejected(Rejection::NonFastForward)
        )]
    );
    assert_eq!(rev_parse(&server, "main")?, server_main, "nothing was sent");

    let outcome = push(&repo, options(&["+rewritten:main"])?)?;
    assert_eq!(statuses(&outcome), [("refs/heads/main".to_owned(), Status::Ok)]);
    assert!(outcome.updates[0].force);
    assert_server_has(dir.path(), "server.git", "main", "rewritten")?;
    assert_eq!(
        repo.find_reference("refs/remotes/origin/main")?.id(),
        repo.rev_parse_single("rewritten")?
    );
    Ok(())
}

#[test]
fn deletions_remove_the_remote_tracking_branch() -> TestResult {
    let dir = fixture()?;
    let repo = client(dir.path(), &local_server(dir.path(), "server.git"), &[])?;
    let outcome = push(&repo, options(&[":other"])?)?;
    assert_eq!(statuses(&outcome), [("refs/heads/other".to_owned(), Status::Ok)]);
    assert!(!refs(&dir.path().join("server.git"))?.contains("refs/heads/other"));
    git(dir.path().join("server.git"), "fsck")?;
    assert!(
        repo.try_find_reference("refs/remotes/origin/other")?.is_none(),
        "the remote-tracking branch is gone too"
    );

    let err = push(&repo, options(&[":other"])?).expect_err("the branch doesn't exist anymore");
    assert!(err.to_string().contains("no such reference"), "{err}");
    Ok(())
}

#[test]
fn existing_tags_need_force() -> TestResult {
    let dir = fixture()?;
    let repo = client(dir.path(), &local_server(dir.path(), "server.git"), &[])?;
    let outcome = push(&repo, options(&["main:refs/tags/light"])?)?;
    assert_eq!(
        statuses(&outcome),
        [("refs/tags/light".to_owned(), Status::Rejected(Rejection::AlreadyExists))]
    );

    let outcome = push(&repo, options(&["+main:refs/tags/light"])?)?;
    assert_eq!(statuses(&outcome), [("refs/tags/light".to_owned(), Status::Ok)]);
    assert_server_has(dir.path(), "server.git", "light", "main")?;
    Ok(())
}

#[test]
fn force_with_lease() -> TestResult {
    let dir = fixture()?;
    let repo = client(dir.path(), &local_server(dir.path(), "server.git"), &[])?;
    let wrong_expectation = repo.rev_parse_single("other")?.detach();
    let mut opts = options(&["rewritten:main"])?;
    opts.leases = vec![Lease {
        ref_name: "main".into(),
        expected: Some(wrong_expectation),
    }];
    let outcome = push(&repo, opts.clone())?;
    assert_eq!(
        statuses(&outcome),
        [("refs/heads/main".to_owned(), Status::Rejected(Rejection::Stale))],
        "the remote doesn't have the expected value"
    );

    opts.leases[0].expected = None;
    let outcome = push(&repo, opts)?;
    assert_eq!(
        statuses(&outcome),
        [("refs/heads/main".to_owned(), Status::Ok)],
        "the remote-tracking branch has the value of the remote, so the lease holds and forces the update"
    );
    assert_server_has(dir.path(), "server.git", "main", "rewritten")?;
    Ok(())
}

#[test]
fn atomic_pushes_fail_as_a_whole() -> TestResult {
    let dir = fixture()?;
    let repo = client(dir.path(), &local_server(dir.path(), "no-deletes.git"), &[])?;
    let server = dir.path().join("no-deletes.git");
    let server_main = rev_parse(&server, "main")?;
    let tracking_main = repo.find_reference("refs/remotes/origin/main")?.id().detach();

    let mut opts = options(&["main", ":other"])?;
    opts.atomic = true;
    let outcome = push(&repo, opts)?;
    assert_eq!(
        statuses(&outcome),
        [
            (
                "refs/heads/main".to_owned(),
                Status::RemoteRejected("atomic push failure".into())
            ),
            (
                "refs/heads/other".to_owned(),
                Status::RemoteRejected("deletion prohibited".into())
            ),
        ],
        "the remote refuses the deletion, and with it all other updates"
    );
    assert_eq!(rev_parse(&server, "main")?, server_main);
    assert!(refs(&server)?.contains("refs/heads/other"));
    git(&server, "fsck")?;
    assert_eq!(
        repo.find_reference("refs/remotes/origin/main")?.id(),
        tracking_main,
        "rejected updates leave remote-tracking branches alone"
    );

    let mut opts = options(&["rewritten:other", "other:main"])?;
    opts.atomic = true;
    let outcome = push(&repo, opts)?;
    assert_eq!(
        statuses(&outcome),
        [
            (
                "refs/heads/other".to_owned(),
                Status::Rejected(Rejection::AtomicPushFailed)
            ),
            (
                "refs/heads/main".to_owned(),
                Status::Rejected(Rejection::NonFastForward)
            ),
        ],
        "local rejections prevent sending anything"
    );
    assert_eq!(rev_parse(&server, "main")?, server_main);
    Ok(())
}

#[test]
fn declining_hooks_are_reported_with_their_messages() -> TestResult {
    let dir = fixture()?;
    let repo = client(dir.path(), &local_server(dir.path(), "hooked.git"), &[])?;
    let server_main = rev_parse(&dir.path().join("hooked.git"), "main")?;
    let outcome = push(&repo, options(&["main"])?)?;
    assert_eq!(
        statuses(&outcome),
        [(
            "refs/heads/main".to_owned(),
            Status::RemoteRejected("pre-receive hook declined".into())
        )]
    );
    assert!(
        outcome.remote_messages.iter().any(|line| line == "main is protected"),
        "hook output is passed on: {:?}",
        outcome.remote_messages
    );
    assert_eq!(rev_parse(&dir.path().join("hooked.git"), "main")?, server_main);
    Ok(())
}

#[test]
fn dry_runs_change_nothing() -> TestResult {
    let dir = fixture()?;
    let repo = client(dir.path(), &local_server(dir.path(), "server.git"), &[])?;
    let server_main = rev_parse(&dir.path().join("server.git"), "main")?;
    let mut opts = options(&["main"])?;
    opts.dry_run = true;
    let outcome = push(&repo, opts)?;
    assert_eq!(statuses(&outcome), [("refs/heads/main".to_owned(), Status::Pending)]);
    assert_eq!(rev_parse(&dir.path().join("server.git"), "main")?, server_main);
    assert_eq!(
        repo.find_reference("refs/remotes/origin/main")?.id().to_string(),
        server_main
    );
    Ok(())
}

#[test]
fn configuration_determines_what_to_push() -> TestResult {
    let dir = fixture()?;
    let url = local_server(dir.path(), "server.git");
    let pushed = |config: &[&str]| -> Result<Vec<(String, Status)>> {
        let repo = client(dir.path(), &url, config)?;
        let outcome = repo
            .find_remote("origin")?
            .connect(Direction::Push)?
            .prepare_push(
                gix::progress::Discard,
                push::Options {
                    dry_run: true,
                    ..Default::default()
                },
            )?
            .send(gix::progress::Discard, &AtomicBool::default())?;
        Ok(statuses(&outcome))
    };
    let main_pending = vec![("refs/heads/main".to_owned(), Status::Pending)];

    let err = pushed(&[]).expect_err("simple needs an upstream");
    assert!(err.to_string().contains("no upstream branch"), "{err}");
    assert_eq!(
        pushed(&["branch.main.remote=origin", "branch.main.merge=refs/heads/main"])?,
        main_pending,
        "simple pushes to the upstream branch of the same name"
    );
    assert_eq!(pushed(&["push.default=current"])?, main_pending);
    assert_eq!(
        pushed(&["push.default=matching"])?,
        [
            ("refs/heads/main".to_owned(), Status::Pending),
            ("refs/heads/other".to_owned(), Status::UpToDate)
        ],
        "branches that exist on both sides"
    );
    let err = pushed(&["push.default=nothing"]).expect_err("nothing is pushed");
    assert!(err.to_string().contains("nothing"), "{err}");
    assert_eq!(
        pushed(&["remote.origin.push=main:refs/heads/configured"])?,
        [("refs/heads/configured".to_owned(), Status::Pending)],
        "push refspecs of the remote take precedence"
    );
    Ok(())
}

#[test]
fn push_urls_take_precedence() -> TestResult {
    let dir = fixture()?;
    let server = local_server(dir.path(), "server.git");
    let repo = client(
        dir.path(),
        &local_server(dir.path(), "hooked.git"),
        &[&format!("remote.origin.pushurl={server}")],
    )?;
    let outcome = push(&repo, options(&["main"])?)?;
    assert_eq!(statuses(&outcome), [("refs/heads/main".to_owned(), Status::Ok)]);
    assert_server_has(dir.path(), "server.git", "main", "main")?;
    Ok(())
}

#[test]
fn git_daemon() -> TestResult {
    let dir = fixture()?;
    let daemon = gix_testtools::spawn_git_daemon(dir.path())?;
    let repo = client(dir.path(), &format!("{}/server.git", daemon.url), &[])?;
    let outcome = push(&repo, options(&["main", ":other"])?)?;
    assert_eq!(
        statuses(&outcome),
        [
            ("refs/heads/main".to_owned(), Status::Ok),
            ("refs/heads/other".to_owned(), Status::Ok)
        ]
    );
    assert_server_has(dir.path(), "server.git", "main", "main")?;
    Ok(())
}

#[test]
#[cfg(unix)]
fn ssh() -> TestResult {
    let dir = fixture()?;
    let ssh = local_server(dir.path(), "bin/ssh");
    let repo = client(
        dir.path(),
        &format!("ssh://localhost{}", local_server(dir.path(), "server.git")),
        &[&format!("core.sshCommand={ssh}")],
    )?;
    let outcome = push(&repo, options(&["main", "v2"])?)?;
    assert_eq!(
        statuses(&outcome),
        [
            ("refs/heads/main".to_owned(), Status::Ok),
            ("refs/tags/v2".to_owned(), Status::Ok)
        ]
    );
    assert_server_has(dir.path(), "server.git", "main", "main")?;
    Ok(())
}

#[test]
#[cfg(feature = "blocking-http-transport-reqwest")]
fn http() -> TestResult {
    let dir = fixture()?;
    let server = gix_testtools::spawn_git_http_backend(dir.path(), None)?;
    let repo = client(dir.path(), &format!("{}/server.git", server.url), &[])?;
    let outcome = push(&repo, options(&["main", ":other", "v2"])?)?;
    assert_eq!(
        statuses(&outcome),
        [
            ("refs/heads/main".to_owned(), Status::Ok),
            ("refs/heads/other".to_owned(), Status::Ok),
            ("refs/tags/v2".to_owned(), Status::Ok)
        ]
    );
    assert_server_has(dir.path(), "server.git", "main", "main")?;
    assert!(!refs(&dir.path().join("server.git"))?.contains("refs/heads/other"));
    Ok(())
}

#[test]
#[cfg(feature = "blocking-http-transport-reqwest")]
fn http_credentials_are_obtained_but_not_sent_in_clear_text() -> TestResult {
    use gix::credentials::{helper::Action, protocol};

    let dir = fixture()?;
    let server = gix_testtools::spawn_git_http_backend(dir.path(), Some(("user", "secret")))?;
    let repo = client(dir.path(), &format!("{}/server.git", server.url), &[])?;
    let mut asked = 0;
    let err = repo
        .find_remote("origin")?
        .connect(Direction::Push)?
        .with_credentials(|action| match action {
            Action::Get(ctx) => {
                asked += 1;
                Ok(Some(protocol::Outcome {
                    credentials: gix::sec::identity::Credentials::UsernamePassword {
                        username: "user".into(),
                        password: "secret".into(),
                        oauth_refresh_token: None,
                    },
                    next: ctx.into(),
                }))
            }
            Action::Store(_) | Action::Erase(_) => Ok(None),
        })
        .prepare_push(gix::progress::Discard, options(&["main"])?)
        .err()
        .expect("credentials are only sent over https");
    assert_eq!(asked, 1, "the server demanded credentials once");
    assert!(format!("{err:?}").contains("clear text"), "{err:?}");
    Ok(())
}
