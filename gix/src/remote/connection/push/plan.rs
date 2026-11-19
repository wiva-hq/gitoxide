use std::collections::BTreeMap;

use gix_error::{OptionExt, ResultExt, bail, message, validation};
use gix_hash::ObjectId;
use gix_protocol::handshake::Ref;
use gix_ref::{FullName, Target};
use gix_refspec::{Instruction, instruction::Push, parse::Operation};

use crate::{
    Remote, Repository, Result,
    bstr::{BStr, BString, ByteSlice},
    push,
    remote::{
        self,
        push::{Lease, Options, Rejection, Status, Update},
    },
};

/// The prefixes to try when expanding an abbreviated reference name, in order.
const PREFIXES: &[&str] = &["", "refs/", "refs/tags/", "refs/heads/", "refs/remotes/"];

/// Return the name and value of an advertised reference, or `None` if it has no value.
pub(super) fn advertised_id(r: &Ref) -> Option<(&BStr, ObjectId)> {
    match r {
        Ref::Direct { full_ref_name, object } => Some((full_ref_name.as_ref(), *object)),
        Ref::Peeled { full_ref_name, tag, .. } => Some((full_ref_name.as_ref(), *tag)),
        Ref::Symbolic {
            full_ref_name,
            tag,
            object,
            ..
        } => Some((full_ref_name.as_ref(), tag.unwrap_or(*object))),
        Ref::Unborn { .. } => None,
    }
}

/// A desired update before checking whether it is allowed.
struct Candidate {
    local_ref: Option<FullName>,
    new_id: Option<ObjectId>,
    remote_ref: FullName,
    force: bool,
}

/// Determine the updates of the references `advertised` by `remote` according to `options`, and reject those
/// which aren't allowed like `git push` does.
pub(super) fn updates(
    remote: &Remote<'_>,
    advertised: &[Ref],
    capabilities: &gix_transport::client::Capabilities,
    options: &Options,
) -> Result<Vec<Update>> {
    let repo = remote.repo;
    // Pseudo-references like `.have` only tell which objects the remote has.
    let remote_refs: BTreeMap<&BStr, ObjectId> = advertised
        .iter()
        .filter_map(advertised_id)
        .filter(|(name, _)| name.starts_with(b"refs/"))
        .collect();
    let specs = if !options.refspecs.is_empty() {
        options.refspecs.clone()
    } else if !remote.push_specs.is_empty() {
        remote.push_specs.clone()
    } else {
        vec![default_refspec(remote)?]
    };
    let mut instructions = Vec::with_capacity(specs.len());
    for spec in &specs {
        let spec = spec.to_ref();
        match spec.instruction() {
            Instruction::Push(push) => instructions.push((push, spec.destination().is_some())),
            Instruction::Fetch(_) => bail!(validation(format!(
                "Refspec {:?} is for fetching, not for pushing",
                spec.to_bstring()
            ))),
        }
    }
    let excludes: Vec<&BStr> = instructions
        .iter()
        .filter_map(|(push, _)| match push {
            Push::Exclude { src } => Some(*src),
            _ => None,
        })
        .collect();
    let is_excluded = |name: &FullName| {
        excludes.iter().any(|pattern| {
            if pattern.contains(&b'*') {
                glob_capture(pattern, name.as_bstr()).is_some()
            } else {
                expansions(pattern).any(|candidate| candidate == name.as_bstr())
            }
        })
    };

    let mut local_refs = None;
    let mut candidates = Vec::new();
    for (push, has_explicit_destination) in instructions {
        match push {
            Push::Exclude { .. } => {}
            Push::AllMatchingBranches { allow_non_fast_forward } => {
                for (name, id) in local_refs_cached(repo, &mut local_refs)? {
                    if name.as_bstr().starts_with(b"refs/heads/")
                        && remote_refs.contains_key(name.as_bstr())
                        && !is_excluded(name)
                    {
                        candidates.push(Candidate {
                            local_ref: Some(name.clone()),
                            new_id: Some(*id),
                            remote_ref: name.clone(),
                            force: allow_non_fast_forward,
                        });
                    }
                }
            }
            Push::Delete { ref_or_pattern } => {
                if ref_or_pattern.contains(&b'*') {
                    bail!(validation(format!(
                        "Cannot delete references by pattern {ref_or_pattern:?}"
                    )));
                }
                let remote_ref = remote_ref_by_name(ref_or_pattern, &remote_refs)?.ok_or_raise(|| {
                    validation(format!(
                        "Cannot delete {ref_or_pattern:?} as the remote has no such reference"
                    ))
                })?;
                candidates.push(Candidate {
                    local_ref: None,
                    new_id: None,
                    remote_ref,
                    force: false,
                });
            }
            Push::Matching {
                src,
                dst,
                allow_non_fast_forward,
            } => {
                if src.contains(&b'*') {
                    if !dst.contains(&b'*') {
                        bail!(validation(format!(
                            "The destination {dst:?} must be a pattern as the source {src:?} is one"
                        )));
                    }
                    for (name, id) in local_refs_cached(repo, &mut local_refs)? {
                        let Some(capture) = glob_capture(src, name.as_bstr()) else {
                            continue;
                        };
                        if is_excluded(name) {
                            continue;
                        }
                        candidates.push(Candidate {
                            local_ref: Some(name.clone()),
                            new_id: Some(*id),
                            remote_ref: full_name(dst.replace(b"*", capture).into())?,
                            force: allow_non_fast_forward,
                        });
                    }
                    continue;
                }
                let (local_ref, new_id) = resolve_source(repo, src)?;
                if local_ref.as_ref().is_some_and(is_excluded) {
                    continue;
                }
                let remote_ref = if !has_explicit_destination {
                    local_ref.clone().ok_or_raise(|| {
                        validation(format!(
                            "A destination is needed to push {src:?} as it isn't a reference, like '{src}:refs/heads/name'"
                        ))
                    })?
                } else if dst.starts_with(b"refs/") {
                    full_name(dst.into())?
                } else {
                    match remote_ref_by_name(dst, &remote_refs)? {
                        Some(name) => name,
                        None => infer_destination(repo, dst, local_ref.as_ref(), new_id)?,
                    }
                };
                candidates.push(Candidate {
                    local_ref,
                    new_id: Some(new_id),
                    remote_ref,
                    force: allow_non_fast_forward,
                });
            }
        }
    }

    let mut unique = Vec::<Candidate>::new();
    for candidate in candidates {
        match unique.iter_mut().find(|c| c.remote_ref == candidate.remote_ref) {
            Some(existing) if existing.new_id == candidate.new_id => existing.force |= candidate.force,
            Some(_) => bail!(validation(format!(
                "Multiple sources would be pushed to {}",
                candidate.remote_ref.as_bstr()
            ))),
            None => unique.push(candidate),
        }
    }

    let null = ObjectId::null(repo.object_hash());
    let mut updates = Vec::with_capacity(unique.len());
    for Candidate {
        local_ref,
        new_id,
        remote_ref,
        mut force,
    } in unique
    {
        let remote_id = remote_refs.get(remote_ref.as_bstr()).copied();
        let tracking_ref = tracking_ref(remote, &remote_ref)?;
        let lease = options
            .leases
            .iter()
            .find(|lease| expansions(lease.ref_name.as_bstr()).any(|name| name == remote_ref.as_bstr()));
        let status = if new_id.is_none() && !capabilities.contains("delete-refs") {
            Status::Rejected(Rejection::DeleteUnsupported)
        } else if remote_id == new_id {
            Status::UpToDate
        } else if let Some(lease) = lease
            && leased_id(repo, lease, tracking_ref.as_ref())? != Some(remote_id.unwrap_or(null))
        {
            Status::Rejected(Rejection::Stale)
        } else {
            force |= lease.is_some();
            match (remote_id, new_id) {
                (Some(remote_id), Some(new_id)) if !force => {
                    match fast_forward_rejection(repo, &remote_ref, remote_id, new_id)? {
                        Some(rejection) => Status::Rejected(rejection),
                        None => Status::Pending,
                    }
                }
                _ => Status::Pending,
            }
        };
        updates.push(Update {
            local_ref,
            new_id,
            remote_ref,
            remote_id,
            tracking_ref,
            force,
            status,
        });
    }

    if options.atomic && updates.iter().any(|u| matches!(u.status, Status::Rejected(_))) {
        for update in &mut updates {
            if update.status == Status::Pending {
                update.status = Status::Rejected(Rejection::AtomicPushFailed);
            }
        }
    }
    Ok(updates)
}

/// Return the refspec that `push.default` implies for pushing the current branch to `remote`.
fn default_refspec(remote: &Remote<'_>) -> Result<gix_refspec::RefSpec> {
    let repo = remote.repo;
    let push_default = repo.push_default()?;
    let spec: BString = if push_default == push::Default::Matching {
        ":".into()
    } else {
        let head = repo
            .head_name()?
            .ok_or_raise(|| validation("Cannot determine what to push as HEAD is detached"))?;
        let branch = head.as_bstr();
        match push_default {
            push::Default::Nothing => bail!(validation(
                "Nothing to push as 'push.default' is 'nothing' and no refspec was given"
            )),
            push::Default::Matching | push::Default::Current => format!("{branch}:{branch}").into(),
            push::Default::Upstream | push::Default::Simple => {
                let fetch_remote = repo.branch_remote_name(head.shorten(), remote::Direction::Fetch);
                let fetch_remote = fetch_remote.as_ref().map_or("origin".into(), remote::Name::as_bstr);
                let is_triangular = remote.name().map(remote::Name::as_bstr) != Some(fetch_remote);
                if is_triangular && push_default == push::Default::Simple {
                    format!("{branch}:{branch}").into()
                } else {
                    if is_triangular {
                        bail!(validation(format!(
                            "Cannot push {branch} to its upstream as the remote isn't the one it fetches from"
                        )));
                    }
                    let upstream = repo
                        .branch_remote_ref_name(head.as_ref(), remote::Direction::Fetch)
                        .transpose()?
                        .ok_or_raise(|| validation(format!("The current branch {branch} has no upstream branch")))?;
                    if push_default == push::Default::Simple && upstream != head {
                        bail!(validation(format!(
                            "The upstream branch {} of {branch} has a different name, which 'push.default=simple' doesn't allow",
                            upstream.as_bstr()
                        )));
                    }
                    format!("{branch}:{}", upstream.as_bstr()).into()
                }
            }
        }
    };
    gix_refspec::parse(spec.as_bstr(), Operation::Push)
        .map(|spec| spec.to_owned())
        .or_raise(|| message!("The refspec {spec:?} derived from 'push.default' is invalid"))
}

/// Return all local references that point to objects directly, computing them once.
fn local_refs_cached<'a>(
    repo: &Repository,
    cache: &'a mut Option<Vec<(FullName, ObjectId)>>,
) -> Result<&'a [(FullName, ObjectId)]> {
    if cache.is_none() {
        let mut refs = Vec::new();
        for reference in repo.references()?.all()? {
            let reference = reference?;
            if let Target::Object(id) = &reference.inner.target {
                refs.push((reference.inner.name.clone(), *id));
            }
        }
        *cache = Some(refs);
    }
    Ok(cache.as_deref().expect("just set"))
}

/// Resolve `src` to the local reference it names, following symbolic references, or to an object.
fn resolve_source(repo: &Repository, src: &BStr) -> Result<(Option<FullName>, ObjectId)> {
    if gix_validate::reference::name_partial(src).is_ok()
        && let Some(mut reference) = repo.try_find_reference(src)?
    {
        // Follow symbolic references like `HEAD` as deep as `git` does.
        for _ in 0..5 {
            match &reference.inner.target {
                Target::Object(id) => return Ok((Some(reference.inner.name.clone()), *id)),
                Target::Symbolic(target) => {
                    let target = target.clone();
                    reference = repo.try_find_reference(&target)?.ok_or_raise(|| {
                        validation(format!(
                            "Cannot push {src:?} as it points to {:?}, which doesn't exist",
                            target.as_bstr()
                        ))
                    })?;
                }
            }
        }
        bail!(validation(format!("Cannot push {src:?} as it is nested too deeply")));
    }
    #[cfg(feature = "revision")]
    let id = repo
        .rev_parse_single(src)
        .or_raise(|| validation(format!("The source {src:?} doesn't match any reference or object")))?
        .detach();
    #[cfg(not(feature = "revision"))]
    let id = ObjectId::from_hex(src)
        .or_raise(|| validation(format!("The source {src:?} doesn't match any reference or object id")))?;
    Ok((None, id))
}

/// Find the unique reference among `remote_refs` that `name` refers to, preferring branches and tags like `git` does.
fn remote_ref_by_name(name: &BStr, remote_refs: &BTreeMap<&BStr, ObjectId>) -> Result<Option<FullName>> {
    let (strong, weak): (Vec<_>, Vec<_>) = expansions(name)
        .filter(|candidate| remote_refs.contains_key(candidate.as_bstr()))
        .partition(|candidate| {
            candidate.len() == name.len()
                || candidate.len() == name.len() + "refs/".len()
                || candidate.starts_with(b"refs/heads/")
                || candidate.starts_with(b"refs/tags/")
        });
    let matches = if strong.is_empty() { weak } else { strong };
    if matches.len() > 1 {
        bail!(validation(format!(
            "The destination {name:?} matches more than one reference on the remote"
        )));
    }
    matches.into_iter().next().map(full_name).transpose()
}

/// Return the remote reference for an abbreviated `dst` that doesn't exist on the remote yet, using the same category
/// as `local_ref`, or as the kind of the object `id` implies.
fn infer_destination(repo: &Repository, dst: &BStr, local_ref: Option<&FullName>, id: ObjectId) -> Result<FullName> {
    let prefix = match local_ref {
        Some(name) if name.as_bstr().starts_with(b"refs/heads/") => Some("refs/heads/"),
        Some(name) if name.as_bstr().starts_with(b"refs/tags/") => Some("refs/tags/"),
        Some(_) => None,
        None => match repo.find_header(id)?.kind() {
            gix_object::Kind::Commit => Some("refs/heads/"),
            gix_object::Kind::Tag => Some("refs/tags/"),
            gix_object::Kind::Tree | gix_object::Kind::Blob => None,
        },
    };
    let prefix = prefix.ok_or_raise(|| {
        validation(format!(
            "The destination {dst:?} must be a full reference name starting with 'refs/' as it doesn't exist on the remote"
        ))
    })?;
    full_name(format!("{prefix}{dst}").into())
}

/// Return the id that `lease` expects, with the null id if the reference must not exist, or `None` if
/// the expectation is unknown as there is no remote-tracking reference.
fn leased_id(repo: &Repository, lease: &Lease, tracking_ref: Option<&FullName>) -> Result<Option<ObjectId>> {
    if let Some(id) = lease.expected {
        return Ok(Some(id));
    }
    let Some(tracking_ref) = tracking_ref else {
        return Ok(None);
    };
    Ok(repo
        .try_find_reference(tracking_ref)?
        .and_then(|reference| reference.target().try_id().map(ToOwned::to_owned)))
}

/// Return why `remote_ref` can't be updated from `remote_id` to `new_id` without force, if it can't.
fn fast_forward_rejection(
    repo: &Repository,
    remote_ref: &FullName,
    remote_id: ObjectId,
    new_id: ObjectId,
) -> Result<Option<Rejection>> {
    if remote_ref.as_bstr().starts_with(b"refs/tags/") {
        return Ok(Some(Rejection::AlreadyExists));
    }
    let Some(remote_header) = repo.try_find_header(remote_id)? else {
        return Ok(Some(Rejection::FetchFirst));
    };
    if remote_header.kind() != gix_object::Kind::Commit || repo.find_header(new_id)?.kind() != gix_object::Kind::Commit
    {
        return Ok(Some(Rejection::NeedsForce));
    }
    for info in repo.rev_walk([new_id]).all()? {
        if info.or_raise(|| message("Could not traverse the commits to push"))?.id == remote_id {
            return Ok(None);
        }
    }
    Ok(Some(Rejection::NonFastForward))
}

/// Return the local reference that tracks `remote_ref` according to the fetch refspecs of `remote`.
fn tracking_ref(remote: &Remote<'_>, remote_ref: &FullName) -> Result<Option<FullName>> {
    let outcome =
        gix_refspec::MatchGroup::from_fetch_specs(remote.fetch_specs.iter().map(gix_refspec::RefSpec::to_ref))
            .match_lhs(std::iter::once(gix_refspec::match_group::Item {
                full_ref_name: remote_ref.as_bstr(),
                target: None,
                object: None,
            }));
    outcome
        .mappings
        .into_iter()
        .find(|mapping| mapping.item_index.is_some())
        .and_then(|mapping| mapping.rhs)
        .map(|name| full_name(name.into_owned()))
        .transpose()
}

/// Return all names that the abbreviated reference `name` may stand for, in order of precedence.
fn expansions(name: &BStr) -> impl Iterator<Item = BString> + '_ {
    PREFIXES.iter().map(move |prefix| {
        let mut expanded = BString::from(*prefix);
        expanded.extend_from_slice(name);
        expanded
    })
}

/// Return the part of `name` that the single `*` in `pattern` stands for, if `name` matches.
fn glob_capture<'a>(pattern: &BStr, name: &'a BStr) -> Option<&'a [u8]> {
    let (prefix, suffix) = pattern.split_once_str(b"*")?;
    name.strip_prefix(prefix)?
        .strip_suffix(suffix)
        .filter(|capture| !capture.is_empty())
}

fn full_name(name: BString) -> Result<FullName> {
    FullName::try_from(name).or_raise(|| validation("Invalid reference name for pushing"))
}
