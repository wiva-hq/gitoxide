use bstr::{BStr, BString, ByteSlice};
use gix_error::{OptionExt, Result, ResultExt, bail, corruption, message};

/// The status report the remote sends after processing all commands, as enabled by the `report-status`
/// or `report-status-v2` capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Report {
    /// The reason the remote couldn't unpack the pack we sent, or `None` if it was unpacked successfully.
    pub unpack_error: Option<BString>,
    /// The status of each reference, in the order the remote sent them.
    pub refs: Vec<RefStatus>,
}

/// The status of a single reference update.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RefStatus {
    /// The full name of the reference as sent in the command.
    pub ref_name: BString,
    /// The reason the remote rejected the update, or `None` if it was applied.
    pub rejection: Option<BString>,
    /// Details the remote sent with `report-status-v2` if it changed the update, which typically happens if
    /// a `proc-receive` hook handled the reference.
    pub options: Options,
}

/// Details about an update applied by the remote, sent only with `report-status-v2`.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Options {
    /// The reference that was actually updated, if it isn't the one in the command.
    pub ref_name: Option<BString>,
    /// The value the reference had before the update, if it isn't the one in the command.
    pub old_id: Option<gix_hash::ObjectId>,
    /// The value the reference has now, if it isn't the one in the command.
    pub new_id: Option<gix_hash::ObjectId>,
    /// If `true`, the update wasn't a fast-forward.
    pub forced_update: bool,
}

impl Report {
    /// Parse the report from the packet `lines` the remote sent, without their trailing newlines.
    ///
    /// Each `ok` line may be followed by `option` lines as used with `report-status-v2`.
    pub fn from_lines<'a>(lines: impl IntoIterator<Item = &'a BStr>) -> Result<Self> {
        let mut lines = lines.into_iter();
        let unpack = lines
            .next()
            .ok_or_raise(|| corruption("The remote sent an empty status report"))?;
        let unpack_error = match unpack.strip_prefix(b"unpack ") {
            Some(b"ok") => None,
            Some(reason) => Some(reason.into()),
            None => bail!(corruption(format!("Expected 'unpack' status, got {unpack:?}"))),
        };

        let mut refs = Vec::<RefStatus>::new();
        for line in lines {
            if let Some(ref_name) = line.strip_prefix(b"ok ") {
                refs.push(RefStatus {
                    ref_name: ref_name.into(),
                    rejection: None,
                    options: Options::default(),
                });
            } else if let Some(rest) = line.strip_prefix(b"ng ") {
                let (ref_name, reason) = rest.split_once_str(b" ").unwrap_or((rest, b""));
                refs.push(RefStatus {
                    ref_name: ref_name.into(),
                    rejection: Some(reason.into()),
                    options: Options::default(),
                });
            } else if let Some(option) = line.strip_prefix(b"option ") {
                let status = refs
                    .last_mut()
                    .filter(|status| status.rejection.is_none())
                    .ok_or_raise(|| corruption(format!("Status option {line:?} doesn't follow an 'ok' line")))?;
                let options = &mut status.options;
                let (key, value) = option.split_once_str(b" ").unwrap_or((option, b""));
                let parse_id = || {
                    gix_hash::ObjectId::from_hex(value)
                        .or_raise(|| message!("Could not parse object id in status option {line:?}"))
                };
                match key {
                    b"refname" => options.ref_name = Some(value.into()),
                    b"old-oid" => options.old_id = Some(parse_id()?),
                    b"new-oid" => options.new_id = Some(parse_id()?),
                    b"forced-update" => options.forced_update = true,
                    _ => bail!(corruption(format!("Unknown status option {line:?}"))),
                }
            } else {
                bail!(corruption(format!("Unexpected line in status report: {line:?}")));
            }
        }
        Ok(Report { unpack_error, refs })
    }
}
