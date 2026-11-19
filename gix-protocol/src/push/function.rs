use std::sync::atomic::{AtomicBool, Ordering};

use bstr::{BString, ByteSlice};
use gix_error::{OptionExt, Result, ResultExt, bail, cancelled, message, unsupported, validation};
use gix_utils::progress::DynNestedProgress;

#[crate::bisync::only_async]
use crate::futures_lite::AsyncWriteExt;
use crate::push::{Command, Context, Options, Outcome, Report};
#[crate::bisync::only_async]
use crate::transport::client::async_io::{ExtendedBufRead, HandleProgress, ReadlineBufRead, Transport};
#[crate::bisync::only_sync]
use crate::transport::client::blocking_io::{ExtendedBufRead, HandleProgress, ReadlineBufRead, Transport};
#[crate::bisync::only_async]
use crate::transport::packetline::async_io::StreamingPeekableIter;
#[crate::bisync::only_sync]
use crate::transport::packetline::blocking_io::StreamingPeekableIter;
use crate::transport::{
    client::{MessageKind, WriteMode},
    packetline::PacketLineRef,
};
#[crate::bisync::only_sync]
use std::io::Write;

/// Send `commands` to the remote on the other side of `transport` to update its references, along with a pack produced
/// by `write_pack(writer, progress, should_interrupt)` that contains all objects the remote needs to apply them.
/// `write_pack` is only called if at least one command isn't a deletion, as the remote expects no pack otherwise.
///
/// The remote must have been contacted with a [`handshake`](crate::handshake()) for
/// [`Service::ReceivePack`](crate::transport::Service::ReceivePack) beforehand.
/// Capabilities are chosen like `git push` does: the report is requested with `report-status-v2` or `report-status`,
/// and progress and messages of the remote are received through `side-band-64k` if available, and passed to a child
/// of `progress`.
///
/// Return the report of the remote along with its messages. Note that the remote rejecting a reference isn't an error,
/// but it's an error if the remote lacks a capability needed for `commands` or `options`, or if it doesn't use the hash
/// of the `commands`.
///
/// The interaction isn't ended, and the remote expects nothing more after this call.
#[crate::bisync::bisync]
pub async fn push<P, T>(
    commands: &[Command],
    write_pack: impl FnOnce(&mut dyn std::io::Write, &mut dyn DynNestedProgress, &AtomicBool) -> Result<()>,
    mut progress: P,
    should_interrupt: &AtomicBool,
    Context {
        handshake,
        transport,
        user_agent,
        trace_packetlines,
    }: Context<'_, T>,
    Options {
        atomic,
        push_options,
        quiet,
    }: Options,
) -> Result<Outcome>
where
    P: gix_utils::progress::NestedProgress,
    P::SubProgress: 'static,
    T: Transport,
{
    let _span = gix_trace::coarse!("gix_protocol::push()", commands = commands.len());
    let Some(first) = commands.first() else {
        bail!(validation("At least one command is needed to push"));
    };
    let capabilities = &handshake.capabilities;
    let remote_object_format = capabilities
        .capability("object-format")
        .and_then(|c| c.value().map(ToOwned::to_owned))
        .unwrap_or_else(|| "sha1".into());
    let object_hash = first.new_id.kind();
    if remote_object_format != object_hash.to_string().as_str() {
        bail!(unsupported(format!(
            "The remote uses {remote_object_format} for object hashes, but the pushed objects use {object_hash}"
        )));
    }
    let report_status = if capabilities.contains("report-status-v2") {
        "report-status-v2"
    } else if capabilities.contains("report-status") {
        "report-status"
    } else {
        bail!(unsupported(
            "The remote doesn't support the 'report-status' capability needed to learn what was updated"
        ));
    };
    if commands.iter().any(Command::is_delete) && !capabilities.contains("delete-refs") {
        bail!(unsupported("The remote doesn't support deleting references"));
    }
    if atomic && !capabilities.contains("atomic") {
        bail!(unsupported("The remote doesn't support atomic pushes"));
    }
    if !push_options.is_empty() && !capabilities.contains("push-options") {
        bail!(unsupported("The remote doesn't support push options"));
    }
    let use_sideband = capabilities.contains("side-band-64k");

    let mut requested = vec![report_status.to_owned()];
    if use_sideband {
        requested.push("side-band-64k".into());
    }
    if quiet && capabilities.contains("quiet") {
        requested.push("quiet".into());
    }
    if atomic {
        requested.push("atomic".into());
    }
    if !push_options.is_empty() {
        requested.push("push-options".into());
    }
    if capabilities.contains("object-format") {
        requested.push(format!("object-format={object_hash}"));
    }
    if let (true, Some(agent)) = (capabilities.contains("agent"), user_agent.1.as_deref()) {
        requested.push(format!("agent={agent}"));
    }
    let request = encode_request(commands, &requested.join(" "), &push_options, trace_packetlines)?;

    if should_interrupt.load(Ordering::Relaxed) {
        bail!(cancelled("Interrupted before sending commands"));
    }
    let progress: &mut dyn DynNestedProgress = &mut progress;
    progress.set_name("push".into());
    // Declared before the reader as its progress handler borrows it.
    let mut remote_messages = Vec::new();
    let (mut writer, mut reader) = transport
        .request(WriteMode::Binary, MessageKind::Flush, trace_packetlines)
        .or_raise(|| message("Could not start the request to the remote"))?
        .into_parts();
    writer
        .write_all(&request)
        .await
        .or_raise(|| message("Could not send commands to the remote"))?;
    if !commands.iter().all(Command::is_delete) {
        write_pack_to(&mut writer, write_pack, &mut *progress, should_interrupt)?;
    }
    writer
        .flush()
        .await
        .or_raise(|| message("Could not send the pack to the remote"))?;
    drop(writer);

    if use_sideband {
        let mut remote_progress =
            progress.add_child_with_id("remote".to_string(), crate::fetch::ProgressId::RemoteProgress.into());
        let messages = &mut remote_messages;
        reader.set_progress_handler(Some(Box::new(move |is_err: bool, text: &[u8]| {
            crate::RemoteProgress::translate_to_progress(is_err, text, &mut remote_progress);
            collect_messages(text, messages);
            if should_interrupt.load(Ordering::Relaxed) {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        }) as HandleProgress<'_>));
    }
    let lines = read_report_lines(&mut reader, use_sideband, trace_packetlines).await?;
    drop(reader);
    let report = Report::from_lines(lines.iter().map(|line| line.as_bstr()))?;
    Ok(Outcome {
        report,
        remote_messages,
    })
}

/// Encode `commands` as packet lines, with `capabilities` on the first one, followed by `push_options`, as `git push` does.
fn encode_request(commands: &[Command], capabilities: &str, push_options: &[BString], trace: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (idx, command) in commands.iter().enumerate() {
        let mut line = BString::from(format!("{} {} {}", command.old_id, command.new_id, command.ref_name));
        if idx == 0 {
            line.push(0);
            line.extend_from_slice(capabilities.as_bytes());
        }
        encode_packet_line(&line, &mut out, trace)?;
    }
    out.extend_from_slice(b"0000");
    if !push_options.is_empty() {
        for option in push_options {
            if option.contains(&b'\n') {
                bail!(validation(format!("Push option {option:?} must not contain a newline")));
            }
            encode_packet_line(option, &mut out, trace)?;
        }
        out.extend_from_slice(b"0000");
    }
    Ok(out)
}

/// The largest amount of data a packet line can hold.
const MAX_DATA_LEN: usize = 65516;

fn encode_packet_line(data: &[u8], out: &mut Vec<u8>, trace: bool) -> Result<()> {
    if data.len() > MAX_DATA_LEN {
        bail!(validation(format!(
            "Line {:?} is too long to be sent as packet line",
            data.as_bstr()
        )));
    }
    if trace {
        gix_trace::trace!(">> {}", data.as_bstr());
    }
    out.extend_from_slice(format!("{:04x}", data.len() + 4).as_bytes());
    out.extend_from_slice(data);
    Ok(())
}

/// Keep lines of `text` sent through a side-band that aren't progress information.
/// Progress lines are terminated with a carriage return to be overwritten by the next one, only their last update
/// is visible and may be followed by more text.
fn collect_messages(text: &[u8], out: &mut Vec<BString>) {
    for line in text.lines() {
        let visible = line.rsplit(|b| *b == b'\r').next().unwrap_or(line).trim();
        if !visible.is_empty() && crate::RemoteProgress::from_bytes(visible).is_none() {
            out.push(visible.into());
        }
    }
}

#[crate::bisync::only_sync]
fn write_pack_to(
    writer: &mut impl std::io::Write,
    write_pack: impl FnOnce(&mut dyn std::io::Write, &mut dyn DynNestedProgress, &AtomicBool) -> Result<()>,
    progress: &mut dyn DynNestedProgress,
    should_interrupt: &AtomicBool,
) -> Result<()> {
    write_pack(writer, progress, should_interrupt).or_raise(|| message("Could not send the pack to the remote"))
}

#[crate::bisync::only_async]
fn write_pack_to(
    writer: &mut (impl crate::futures_io::AsyncWrite + Unpin),
    write_pack: impl FnOnce(&mut dyn std::io::Write, &mut dyn DynNestedProgress, &AtomicBool) -> Result<()>,
    progress: &mut dyn DynNestedProgress,
    should_interrupt: &AtomicBool,
) -> Result<()> {
    let mut writer = crate::futures_lite::io::BlockOn::new(writer);
    write_pack(&mut writer, progress, should_interrupt).or_raise(|| message("Could not send the pack to the remote"))
}

/// Read the status report, which is nested into the data channel if `use_sideband` is `true`, and return its lines.
#[crate::bisync::bisync]
async fn read_report_lines<'a>(
    reader: &mut Box<dyn ExtendedBufRead<'a> + Unpin + 'a>,
    use_sideband: bool,
    trace: bool,
) -> Result<Vec<BString>> {
    let mut lines = Vec::new();
    if use_sideband {
        let mut nested = StreamingPeekableIter::new(&mut *reader, &[PacketLineRef::Flush], trace);
        while let Some(line) = nested.read_line().await {
            lines.push(to_text(line)?);
        }
        if nested.stopped_at().is_none() {
            bail!(message("The remote hung up before sending its complete status report"));
        }
        drop(nested);
        read_remaining(reader)
            .await
            .or_raise(|| message("Could not read the end of the remote response"))?;
    } else {
        while let Some(line) = reader.readline().await {
            lines.push(to_text(line)?);
        }
        if reader.stopped_at().is_none() {
            bail!(message("The remote hung up before sending its complete status report"));
        }
    }
    Ok(lines)
}

fn to_text(line: std::io::Result<Result<PacketLineRef<'_>>>) -> Result<BString> {
    let line = line
        .or_raise(|| message("Could not read the status report"))?
        .or_raise(|| message("Could not decode the status report"))?;
    let data = line
        .as_slice()
        .ok_or_raise(|| message("Expected a data line in the status report"))?;
    Ok(data.strip_suffix(b"\n").unwrap_or(data).into())
}

#[crate::bisync::only_async]
async fn read_remaining(reader: &mut (impl crate::futures_io::AsyncRead + Unpin)) -> std::io::Result<()> {
    crate::futures_lite::io::copy(reader, &mut crate::futures_lite::io::sink())
        .await
        .map(|_| ())
}

#[crate::bisync::only_sync]
fn read_remaining(reader: &mut impl std::io::Read) -> std::io::Result<()> {
    std::io::copy(reader, &mut std::io::sink()).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::collect_messages;

    #[test]
    fn messages_exclude_progress() {
        let mut out = Vec::new();
        collect_messages(
            b"Resolving deltas:  50% (1/2)\rResolving deltas: 100% (2/2), done.",
            &mut out,
        );
        collect_messages(b"main is protected", &mut out);
        collect_messages(b"error: first\nsecond\n", &mut out);
        assert_eq!(
            out,
            ["main is protected", "error: first", "second"],
            "progress lines are dropped, all other lines are kept individually"
        );
    }
}
