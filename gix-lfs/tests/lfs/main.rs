mod client;
mod pointer;
mod store;

fn content_pointer(content: &[u8]) -> gix_lfs::Pointer {
    gix_lfs::Pointer::new(gix_lfs::Oid::for_content(content), content.len() as u64)
}
