//! Golden files: what the operator renders, committed under
//! `testdata/golden/`, so a change to the output shows in review.
//! `UPDATE_GOLDEN=1 cargo test -p application` (`task golden:update`)
//! rewrites them.

use std::path::PathBuf;

/// Compares `actual` with `testdata/golden/<name>`, or writes it there
/// when `UPDATE_GOLDEN` is set.
pub fn check(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/golden")
        .join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (UPDATE_GOLDEN=1 writes it)", path.display()));
    assert!(
        expected == actual,
        "{name} differs from its golden file (UPDATE_GOLDEN=1 rewrites it):\n--- expected\n{expected}\n--- actual\n{actual}"
    );
}
