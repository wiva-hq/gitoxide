use bstr::BString;
use gix_protocol::push::{self, Command, Report, report};

use crate::fetch::oid;

const MAIN_OLD: &str = "af1bbe06b8b922784b44be8e02df34e5da7997af";
const MAIN_NEW: &str = "a32f5c9f2342f2ee9f6ab8c8b27a334d1594d357";
const NULL: &str = "0000000000000000000000000000000000000000";
const PACK: &[u8] = b"PACK-STAND-IN";

fn command(old: &str, new: &str, ref_name: &str) -> Command {
    Command {
        ref_name: ref_name.into(),
        old_id: oid(old),
        new_id: oid(new),
    }
}

fn no_credentials(
    _action: gix_protocol::credentials::helper::Action,
) -> gix_error::Result<Option<gix_protocol::credentials::protocol::Outcome>> {
    unreachable!("the remote never asks for credentials")
}

fn pkt(data: &str) -> String {
    format!("{:04x}{data}", data.len() + 4)
}

fn ok(ref_name: &str) -> report::RefStatus {
    report::RefStatus {
        ref_name: ref_name.into(),
        rejection: None,
        options: Default::default(),
    }
}

fn rejected(ref_name: &str, reason: &str) -> report::RefStatus {
    report::RefStatus {
        ref_name: ref_name.into(),
        rejection: Some(reason.into()),
        options: Default::default(),
    }
}

/// Perform the handshake and push `commands` against a response that `git receive-pack` produced,
/// and return the outcome along with everything that was written to the remote.
#[crate::bisync::bisync]
async fn push_with_fixture(
    fixture: &str,
    commands: &[Command],
    options: push::Options,
) -> gix_error::Result<(push::Outcome, BString)> {
    use gix_error::ResultExt;
    use gix_transport::{Protocol, Service, client::git::ConnectMode};

    let mut transport = crate::fetch::transport(Vec::new(), fixture, Protocol::V1, ConnectMode::Process);
    let handshake = gix_protocol::handshake(
        &mut transport,
        Service::ReceivePack,
        no_credentials,
        Vec::new(),
        &mut gix_utils::progress::Discard,
    )
    .await?;
    assert!(handshake.refs.is_some(), "receive-pack always advertises its refs");
    let outcome = gix_protocol::push(
        commands,
        |out, _progress, _should_interrupt| out.write_all(PACK).or_error(),
        gix_utils::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false),
        push::Context {
            handshake: &handshake,
            transport: &mut transport,
            user_agent: ("agent", Some("git/gix-test".into())),
            trace_packetlines: true,
        },
        options,
    )
    .await?;
    Ok((outcome, transport.into_inner().1.into()))
}

#[crate::bisync::bisync]
#[cfg_attr(feature = "blocking-client", test)]
#[cfg_attr(all(feature = "async-client", not(feature = "blocking-client")), async_std::test)]
async fn update_delete_and_create_with_sideband() -> crate::TestResult {
    let commands = [
        command(MAIN_OLD, MAIN_NEW, "refs/heads/main"),
        command(MAIN_OLD, NULL, "refs/heads/other"),
        command(NULL, MAIN_NEW, "refs/heads/topic"),
    ];
    let (outcome, written) = push_with_fixture("v1/push-update.response", &commands, Default::default()).await?;
    let mut expected = BString::from(format!(
        "{}{}{}0000",
        pkt(&format!(
            "{MAIN_OLD} {MAIN_NEW} refs/heads/main\0report-status-v2 side-band-64k object-format=sha1 agent=git/gix-test"
        )),
        pkt(&format!("{MAIN_OLD} {NULL} refs/heads/other")),
        pkt(&format!("{NULL} {MAIN_NEW} refs/heads/topic")),
    ));
    expected.extend_from_slice(PACK);
    assert_eq!(
        written, expected,
        "capabilities ride on the first command, the pack follows the flush packet verbatim"
    );
    assert_eq!(
        outcome.report,
        Report {
            unpack_error: None,
            refs: vec![ok("refs/heads/main"), ok("refs/heads/other"), ok("refs/heads/topic")]
        },
        "the report nested in the side-band is decoded"
    );
    assert!(outcome.remote_messages.is_empty(), "the remote had nothing to say");
    Ok(())
}

#[crate::bisync::bisync]
#[cfg_attr(feature = "blocking-client", test)]
#[cfg_attr(all(feature = "async-client", not(feature = "blocking-client")), async_std::test)]
async fn declining_hook_messages_are_surfaced() -> crate::TestResult {
    let commands = [command(MAIN_OLD, MAIN_NEW, "refs/heads/main")];
    let (outcome, written) = push_with_fixture(
        "v1/push-hook-declined.response",
        &commands,
        push::Options {
            quiet: true,
            ..Default::default()
        },
    )
    .await?;
    assert!(
        written.starts_with(
            pkt(&format!(
                "{MAIN_OLD} {MAIN_NEW} refs/heads/main\0report-status-v2 side-band-64k quiet object-format=sha1 agent=git/gix-test"
            ))
            .as_bytes()
        ),
        "quiet is requested as the remote supports it"
    );
    assert_eq!(
        outcome.report.refs,
        [rejected("refs/heads/main", "pre-receive hook declined")],
        "the reason of the remote is passed on"
    );
    assert_eq!(
        outcome.remote_messages,
        ["main is protected"],
        "hook output arrives through the progress side-band"
    );
    Ok(())
}

#[crate::bisync::bisync]
#[cfg_attr(feature = "blocking-client", test)]
#[cfg_attr(all(feature = "async-client", not(feature = "blocking-client")), async_std::test)]
async fn atomic_push_without_sideband_is_rejected_as_a_whole() -> crate::TestResult {
    // The advertisement was captured from `git receive-pack` and edited to not offer `side-band-64k`,
    // which is why the report arrives as plain packet lines.
    let commands = [
        command(MAIN_OLD, MAIN_NEW, "refs/heads/main"),
        command(MAIN_OLD, NULL, "refs/heads/other"),
    ];
    let (outcome, written) = push_with_fixture(
        "v1/push-atomic-rejected.response",
        &commands,
        push::Options {
            atomic: true,
            ..Default::default()
        },
    )
    .await?;
    assert!(
        written.starts_with(
            pkt(&format!(
                "{MAIN_OLD} {MAIN_NEW} refs/heads/main\0report-status-v2 atomic object-format=sha1 agent=git/gix-test"
            ))
            .as_bytes()
        ),
        "atomic is requested, side-band isn't"
    );
    assert_eq!(
        outcome.report.refs,
        [
            rejected("refs/heads/main", "atomic push failure"),
            rejected("refs/heads/other", "deletion prohibited")
        ],
        "one rejected deletion fails all updates"
    );
    Ok(())
}

#[crate::bisync::bisync]
#[cfg_attr(feature = "blocking-client", test)]
#[cfg_attr(all(feature = "async-client", not(feature = "blocking-client")), async_std::test)]
async fn deletions_send_no_pack() -> crate::TestResult {
    let commands = [command(MAIN_OLD, NULL, "refs/heads/other")];
    let (outcome, written) = push_with_fixture("v1/push-delete.response", &commands, Default::default()).await?;
    assert_eq!(
        written,
        format!(
            "{}0000",
            pkt(&format!(
                "{MAIN_OLD} {NULL} refs/heads/other\0report-status-v2 object-format=sha1 agent=git/gix-test"
            ))
        ),
        "the remote expects no pack if all commands are deletions"
    );
    assert_eq!(outcome.report.refs, [ok("refs/heads/other")]);
    Ok(())
}

#[crate::bisync::bisync]
#[cfg_attr(feature = "blocking-client", test)]
#[cfg_attr(all(feature = "async-client", not(feature = "blocking-client")), async_std::test)]
async fn create_in_empty_repository() -> crate::TestResult {
    let commands = [command(NULL, MAIN_NEW, "refs/heads/main")];
    let (outcome, written) = push_with_fixture("v1/push-to-empty.response", &commands, Default::default()).await?;
    assert!(written.ends_with(PACK), "objects are sent to create the reference");
    assert_eq!(outcome.report.refs, [ok("refs/heads/main")]);

    let err = push_with_fixture(
        "v1/push-to-empty.response",
        &commands,
        push::Options {
            push_options: vec!["ci.skip".into()],
            ..Default::default()
        },
    )
    .await
    .expect_err("the remote doesn't advertise push-options");
    assert!(
        err.to_string().contains("push options"),
        "missing capabilities are detected before sending anything: {err}"
    );
    Ok(())
}

mod report_parsing {
    use bstr::BStr;
    use gix_protocol::push::{Report, report};

    use super::{MAIN_NEW, MAIN_OLD, ok, rejected};
    use crate::fetch::oid;

    fn parse(input: &str) -> gix_error::Result<Report> {
        Report::from_lines(input.lines().map(BStr::new))
    }

    #[test]
    fn v2_options_amend_the_preceding_update() -> crate::TestResult {
        let report = parse(&format!(
            "unpack ok
ok refs/for/main/topic
option refname refs/changes/1/1
option old-oid {MAIN_OLD}
option new-oid {MAIN_NEW}
option forced-update
ok refs/heads/main
ng refs/heads/next non-fast-forward, rejected"
        ))?;
        assert_eq!(
            report,
            Report {
                unpack_error: None,
                refs: vec![
                    report::RefStatus {
                        ref_name: "refs/for/main/topic".into(),
                        rejection: None,
                        options: report::Options {
                            ref_name: Some("refs/changes/1/1".into()),
                            old_id: Some(oid(MAIN_OLD)),
                            new_id: Some(oid(MAIN_NEW)),
                            forced_update: true,
                        },
                    },
                    ok("refs/heads/main"),
                    rejected("refs/heads/next", "non-fast-forward, rejected"),
                ]
            },
            "options belong to the `ok` line before them, reasons may contain spaces"
        );
        Ok(())
    }

    #[test]
    fn unpack_errors_are_reported() -> crate::TestResult {
        let report = parse("unpack index-pack abnormal exit\nng refs/heads/main unpacker error")?;
        assert_eq!(report.unpack_error, Some("index-pack abnormal exit".into()));
        assert_eq!(report.refs, [rejected("refs/heads/main", "unpacker error")]);
        Ok(())
    }

    #[test]
    fn malformed_reports_are_errors() {
        for input in [
            "",
            "ok refs/heads/main",
            "unpack ok\nng refs/heads/main reason\noption forced-update",
            "unpack ok\noption forced-update",
            "unpack ok\nok refs/heads/main\noption unknown-option",
            "unpack ok\nok refs/heads/main\noption new-oid not-a-hash",
            "unpack ok\nunexpected",
        ] {
            assert!(parse(input).is_err(), "{input:?} must be rejected");
        }
    }
}
