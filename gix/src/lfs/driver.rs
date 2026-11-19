//! The built-in `lfs` filter driver.
use std::io::{Cursor, Read};

use gix_filter::driver::{Operation, apply::Context, builtin};
use gix_lfs::filter::{Smudged, clean, smudge};

use crate::{
    Repository, Result,
    config::{cache::util::ApplyLeniency, tree::gitoxide},
    error::{ResultExt, message},
};

/// Add the built-in `lfs` driver to `pipeline` unless `gitoxide.core.builtinLfsFilter` is `false`.
pub(crate) fn register(repo: &Repository, mut pipeline: gix_filter::Pipeline) -> Result<gix_filter::Pipeline> {
    let key = &gitoxide::Core::BUILTIN_LFS_FILTER;
    let config = &repo.config.resolved;
    let is_enabled = key
        .enrich_error(config.boolean_filter(key, &mut repo.filter_config_section()))
        .with_leniency(repo.config.lenient_config)?
        .unwrap_or(true);
    if !is_enabled {
        return Ok(pipeline);
    }
    let driver = Driver {
        store: repo.lfs().store()?,
        #[cfg(all(feature = "blocking-http-transport-reqwest", feature = "parallel"))]
        downloads: downloads::Downloads::new(repo)?,
    };
    pipeline.builtin_drivers_mut().push(builtin::Builtin {
        name: "lfs".into(),
        filter: std::sync::Arc::new(driver),
    });
    Ok(pipeline)
}

struct Driver {
    store: gix_lfs::Store,
    #[cfg(all(feature = "blocking-http-transport-reqwest", feature = "parallel"))]
    downloads: downloads::Downloads,
}

impl builtin::Filter for Driver {
    fn apply(&self, src: &mut dyn Read, operation: Operation, ctx: Context<'_, '_>) -> Result<Box<dyn Read>> {
        match operation {
            Operation::Clean => Ok(Box::new(Cursor::new(clean(&self.store, src)?))),
            Operation::Smudge => {
                let mut input = Vec::new();
                src.read_to_end(&mut input)
                    .or_raise(|| message!("Could not read the LFS pointer of '{}'", ctx.rela_path))?;
                Ok(match smudge(&self.store, &input)? {
                    Smudged::NotAPointer => Box::new(Cursor::new(input)),
                    Smudged::Empty => Box::new(std::io::empty()),
                    Smudged::Object(file) => Box::new(file),
                    Smudged::Missing(pointer) => self.missing(&pointer, input, ctx)?,
                })
            }
        }
    }
}

impl Driver {
    /// Download the object of `pointer` if possible, or keep `input`, the pointer itself.
    fn missing(&self, pointer: &gix_lfs::Pointer, input: Vec<u8>, ctx: Context<'_, '_>) -> Result<Box<dyn Read>> {
        // Without tracing, the warning below doesn't use them.
        let _ = (pointer, &ctx);
        #[cfg(all(feature = "blocking-http-transport-reqwest", feature = "parallel"))]
        match self.downloads.fetch(&self.store, pointer) {
            Ok(true) => {
                if let Some(file) = self.store.open(pointer)? {
                    return Ok(Box::new(file));
                }
            }
            Ok(false) => {}
            Err(_err) if self.downloads.skip_errors => {
                gix_trace::warn!(
                    "Could not download LFS object {} of '{}', leaving its pointer in place: {_err}",
                    pointer.oid,
                    ctx.rela_path
                );
                return Ok(Box::new(Cursor::new(input)));
            }
            Err(err) => {
                return Err(err).or_raise(|| message!("Could not download the LFS object of '{}'", ctx.rela_path));
            }
        }
        gix_trace::warn!(
            "LFS object {} of '{}' is not available, leaving its pointer in place",
            pointer.oid,
            ctx.rela_path
        );
        Ok(Box::new(Cursor::new(input)))
    }
}

#[cfg(all(feature = "blocking-http-transport-reqwest", feature = "parallel"))]
mod downloads {
    use std::sync::{Arc, Mutex, PoisonError, atomic::AtomicBool};

    use crate::{Result, config::cache::util::ApplyLeniency};

    /// Downloads of missing objects from the default remote, with the client created on first use.
    pub(super) struct Downloads {
        /// If `true`, as configured with `lfs.skipDownloadErrors`, failed downloads leave the pointer in place.
        pub(super) skip_errors: bool,
        repo: crate::ThreadSafeRepository,
        client: Mutex<Option<Arc<gix_lfs::Client>>>,
    }

    impl Downloads {
        pub(super) fn new(repo: &crate::Repository) -> Result<Self> {
            let skip_errors = repo
                .config
                .resolved
                .boolean_filter("lfs.skipDownloadErrors", &mut repo.filter_config_section())
                .with_leniency(repo.config.lenient_config)?
                .unwrap_or(false);
            Ok(Downloads {
                skip_errors,
                repo: repo.clone().into_sync(),
                client: Mutex::new(None),
            })
        }

        /// Download the object of `pointer` into `store`, or return `false` if there is no remote to download it from.
        pub(super) fn fetch(&self, store: &gix_lfs::Store, pointer: &gix_lfs::Pointer) -> Result<bool> {
            let Some(client) = self.client()? else {
                return Ok(false);
            };
            let outcome = client.download(
                store,
                std::slice::from_ref(pointer),
                &mut gix_utils::progress::Discard,
                &AtomicBool::new(false),
            )?;
            if let Some(failure) = outcome.failed.into_iter().next() {
                return Err(failure.error);
            }
            Ok(true)
        }

        fn client(&self) -> Result<Option<Arc<gix_lfs::Client>>> {
            let mut client = self.client.lock().unwrap_or_else(PoisonError::into_inner);
            if client.is_none() {
                let repo = self.repo.to_thread_local();
                *client = repo.lfs().client(None, gix_lfs::Operation::Download)?.map(Arc::new);
            }
            Ok(client.clone())
        }
    }
}
