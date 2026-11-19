use std::sync::Arc;

use bstr::BString;
use gix_error::Result;

use crate::driver::{Operation, apply::Context};

/// A filter driver implemented in-process.
///
/// Register it with [`Pipeline::builtin_drivers_mut()`](crate::Pipeline::builtin_drivers_mut()) to apply it to paths
/// whose `filter` attribute names it, in place of any [program driver](crate::Driver) of the same name.
pub trait Filter: Send + Sync {
    /// Apply `operation` to `src`, the content of the entry described by `ctx`, and return a reader for the result.
    ///
    /// The result can't borrow from `src`, which must be consumed as far as needed before returning.
    fn apply(
        &self,
        src: &mut dyn std::io::Read,
        operation: Operation,
        ctx: Context<'_, '_>,
    ) -> Result<Box<dyn std::io::Read>>;
}

/// A [`Filter`] together with the driver name that `filter` attributes refer to.
#[derive(Clone)]
pub struct Builtin {
    /// The name of the driver, as in `filter=<name>`.
    pub name: BString,
    /// The implementation of the driver.
    pub filter: Arc<dyn Filter>,
}
