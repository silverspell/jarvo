pub mod auth;
pub mod client;
pub mod protocol;
pub mod registry;
pub mod server;
pub mod spawn;

use std::io;
use std::path::{Path, PathBuf};

/// Resolve the office directory once, so the server, its registry and every
/// spawned agent agree on it even when the user typed a relative path.
pub(crate) fn absolute_dir(dir: &Path) -> io::Result<PathBuf> {
    std::path::absolute(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_office_dir_is_made_absolute() {
        let relative = absolute_dir(Path::new("jarvo-office-test")).expect("resolve");
        assert!(relative.is_absolute());
        assert!(relative.ends_with("jarvo-office-test"));

        let absolute = absolute_dir(Path::new("/tmp/jarvo-office")).expect("resolve");
        assert_eq!(absolute, Path::new("/tmp/jarvo-office"));
    }
}
