impl crate::Repository {
    /// Return a platform to work with Git LFS objects.
    pub fn lfs(&self) -> crate::lfs::Platform<'_> {
        crate::lfs::Platform { repo: self }
    }
}
