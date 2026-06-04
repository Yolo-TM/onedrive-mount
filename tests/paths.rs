// Unit tests for path utilities

use onedrive_mount::paths::expand_tilde;

#[test]
fn expand_tilde_home() {
    let home = dirs::home_dir().expect("home dir must be set in test environment");
    let expanded = expand_tilde("~/documents");
    assert_eq!(expanded, home.join("documents"));
}

#[test]
fn expand_tilde_alone() {
    let home = dirs::home_dir().expect("home dir must be set in test environment");
    assert_eq!(expand_tilde("~"), home);
}

#[test]
fn expand_tilde_no_tilde() {
    let path = "/absolute/path";
    assert_eq!(expand_tilde(path), std::path::PathBuf::from(path));
}

#[test]
fn expand_tilde_relative_path() {
    let path = "relative/path";
    assert_eq!(expand_tilde(path), std::path::PathBuf::from(path));
}

#[test]
fn expand_tilde_tilde_in_middle_not_expanded() {
    // Only a leading ~ is expanded
    let path = "/some/~/path";
    assert_eq!(expand_tilde(path), std::path::PathBuf::from(path));
}
