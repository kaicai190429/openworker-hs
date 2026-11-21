//! Which paths `media_upload` may hand to HeliconSearch.
//!
//! HeliconSearch reads and indexes whatever path it is given (a folder is walked, its images, videos and
//! documents are indexed), as the user it runs as, and a search then returns what it read. A model that
//! was talked into naming `~/payroll` would get it indexed, so only paths inside the folders of
//! `mediaUploadRoots` go through.
//!
//! A path is judged by where it really is on this machine, so a symlink inside an allowed folder that points
//! out of it does not count. A path with `..` in it is refused: `link/..` is not the folder that holds `link`
//! when `link` is a symlink, and the file system, not the text, decides where it goes. The part of a path that
//! does not exist (a file that is not there yet, a path that exists only on another host) is judged by its
//! text, below the real path of the part that does. HeliconSearch reads the path some time after the check:
//! this keeps the model from naming a path, it is not a sandbox.
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// Whether `path` is absolute and is, or is inside, one of the `roots`.
pub(crate) fn upload_path_allowed(path: &str, roots: &[String]) -> bool {
    // (a root is the operator's own: `..` in it is resolved)
    if Path::new(path)
        .components()
        .any(|part| part == Component::ParentDir)
    {
        return false;
    }
    let Some(real) = resolve(path) else {
        return false;
    };
    roots
        .iter()
        .filter_map(|root| resolve(root))
        .any(|root| inside(&real, &root))
}

/// The absolute, normalised path with the real path of its existing part; `None` for a path that is not
/// absolute, holds a NUL, or runs into a folder this process may not look into.
fn resolve(raw: &str) -> Option<PathBuf> {
    if raw.contains('\0') || !Path::new(raw).is_absolute() {
        return None;
    }
    let mut lexical = PathBuf::new();
    for part in Path::new(raw).components() {
        match part {
            Component::Prefix(_) | Component::RootDir => lexical.push(part.as_os_str()),
            Component::CurDir => {}
            // (above the root there is nothing to go up to)
            Component::ParentDir => {
                if matches!(lexical.components().next_back(), Some(Component::Normal(_))) {
                    lexical.pop();
                }
            }
            Component::Normal(name) => lexical.push(name),
        }
    }
    let mut base = lexical.clone();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&base) {
            Ok(real) => {
                let mut out = simplify(real);
                out.extend(tail.iter().rev());
                return Some(out);
            }
            Err(e) if e.kind() == ErrorKind::PermissionDenied => return None,
            // not there (or a file where a folder was expected): look at the folder above
            Err(_) => match base.file_name().map(OsString::from) {
                Some(name) => {
                    tail.push(name);
                    base.pop();
                }
                // nothing of it exists on this machine
                None => return Some(lexical),
            },
        }
    }
}

/// `\\?\C:\x` is the same place as `C:\x`: the real path of a Windows folder comes with the first, a path
/// that is not there yet is written with the second.
#[cfg(windows)]
fn simplify(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(not(windows))]
fn simplify(path: PathBuf) -> PathBuf {
    path
}

/// File names do not differ by case on Windows.
#[cfg(windows)]
fn fold(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(not(windows))]
fn fold(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// By whole components: `/data/media-evil` is not inside `/data/media`.
fn inside(path: &Path, root: &Path) -> bool {
    fold(path).starts_with(fold(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    fn root_and_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("media");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(root.join("trip")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(root.join("a.jpg"), b"x").unwrap();
        std::fs::write(other.join("secret.xlsx"), b"x").unwrap();
        (tmp, root, other)
    }

    #[test]
    fn a_file_a_folder_and_the_folder_itself_inside_a_root_are_allowed() {
        let (_tmp, root, _) = root_and_dirs();
        let roots = vec![s(&root)];
        assert!(upload_path_allowed(&s(&root.join("a.jpg")), &roots));
        assert!(upload_path_allowed(&s(&root.join("trip")), &roots));
        assert!(upload_path_allowed(&s(&root), &roots));
        // a trailing separator on either side changes nothing
        let slash = std::path::MAIN_SEPARATOR;
        assert!(upload_path_allowed(
            &format!("{}{slash}", s(&root.join("trip"))),
            &[format!("{}{slash}", s(&root))]
        ));
    }

    #[test]
    fn nothing_is_allowed_without_a_root() {
        let (_tmp, root, _) = root_and_dirs();
        assert!(!upload_path_allowed(&s(&root.join("a.jpg")), &[]));
    }

    #[test]
    fn a_path_outside_every_root_is_refused() {
        let (_tmp, root, other) = root_and_dirs();
        let roots = vec![s(&root)];
        assert!(!upload_path_allowed(&s(&other.join("secret.xlsx")), &roots));
        assert!(!upload_path_allowed(&s(&other), &roots));
        // (the folder above the root is not inside it)
        assert!(!upload_path_allowed(&s(root.parent().unwrap()), &roots));
    }

    #[test]
    fn a_sibling_that_starts_with_the_name_of_a_root_is_not_inside_it() {
        let (tmp, root, _) = root_and_dirs();
        let evil = tmp.path().join("media-evil");
        std::fs::create_dir_all(&evil).unwrap();
        std::fs::write(evil.join("x.jpg"), b"x").unwrap();
        assert!(!upload_path_allowed(&s(&evil.join("x.jpg")), &[s(&root)]));
    }

    #[test]
    fn a_path_with_dot_dot_in_it_is_refused_wherever_it_ends_up() {
        let (tmp, root, other) = root_and_dirs();
        let roots = vec![s(&root)];
        let slash = std::path::MAIN_SEPARATOR;
        let climbing = format!("{}{slash}..{slash}other{slash}secret.xlsx", s(&root));
        assert!(!upload_path_allowed(&climbing, &roots));
        // not even where it comes back into the folder
        let back_in = format!("{}{slash}trip{slash}..{slash}a.jpg", s(&root));
        assert!(!upload_path_allowed(&back_in, &roots));
        assert!(other.join("secret.xlsx").exists());
        let deep = format!("{}{}", s(&root), format!("{slash}..").repeat(40));
        assert!(!upload_path_allowed(&deep, &roots));
        // a `.` is only the folder itself
        let dot = format!("{}{slash}.{slash}a.jpg", s(&root));
        assert!(upload_path_allowed(&dot, &roots));
        // a root the operator wrote with `..` in it is the folder it names
        let written = format!("{}{slash}trip{slash}..", s(&root));
        assert!(upload_path_allowed(&s(&root.join("a.jpg")), &[written]));
        assert!(!upload_path_allowed(
            &s(&other.join("secret.xlsx")),
            &[format!("{}{slash}trip{slash}..", s(&root))]
        ));
        drop(tmp);
    }

    #[test]
    fn a_relative_path_and_a_nul_are_refused() {
        let (_tmp, root, _) = root_and_dirs();
        let roots = vec![s(&root)];
        assert!(!upload_path_allowed("a.jpg", &roots));
        assert!(!upload_path_allowed("media/a.jpg", &roots));
        assert!(!upload_path_allowed("", &roots));
        assert!(!upload_path_allowed(
            &format!("{}\0/a.jpg", s(&root)),
            &roots
        ));
        // (and a root that is not absolute allows nothing)
        assert!(!upload_path_allowed(
            &s(&root.join("a.jpg")),
            &["media".into()]
        ));
    }

    #[test]
    fn a_file_that_is_not_there_yet_is_judged_by_where_it_would_be() {
        let (_tmp, root, other) = root_and_dirs();
        let roots = vec![s(&root)];
        assert!(upload_path_allowed(
            &s(&root.join("new").join("b.jpg")),
            &roots
        ));
        assert!(!upload_path_allowed(
            &s(&other.join("new").join("b.jpg")),
            &roots
        ));
        // a file where a folder was expected
        assert!(upload_path_allowed(
            &s(&root.join("a.jpg").join("x")),
            &roots
        ));
    }

    #[test]
    fn a_root_that_does_not_exist_yet_still_works_as_text() {
        let tmp = tempfile::tempdir().unwrap();
        let later = tmp.path().join("mounted-later");
        let roots = vec![s(&later)];
        assert!(upload_path_allowed(&s(&later.join("a.jpg")), &roots));
        assert!(!upload_path_allowed(&s(&tmp.path().join("a.jpg")), &roots));
    }

    #[test]
    fn any_of_several_roots_is_enough() {
        let (_tmp, root, other) = root_and_dirs();
        let roots = vec![s(&root), s(&other)];
        assert!(upload_path_allowed(&s(&other.join("secret.xlsx")), &roots));
        assert!(upload_path_allowed(&s(&root.join("a.jpg")), &roots));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_root_is_refused_and_a_symlinked_root_is_followed() {
        use std::os::unix::fs::symlink;
        let (tmp, root, other) = root_and_dirs();
        // `link/..` is the folder above the link's target, not the folder that holds the link
        std::fs::create_dir_all(other.join("deep")).unwrap();
        symlink(other.join("deep"), root.join("link")).unwrap();
        let through = format!("{}/link/../secret.xlsx", s(&root));
        assert!(
            std::path::Path::new(&through).exists(),
            "the kernel reads {through}"
        );
        assert!(!upload_path_allowed(&through, &[s(&root)]));
        // a link inside the root to a file and to a folder outside it
        symlink(other.join("secret.xlsx"), root.join("link.xlsx")).unwrap();
        symlink(&other, root.join("linked-folder")).unwrap();
        let roots = vec![s(&root)];
        assert!(!upload_path_allowed(&s(&root.join("link.xlsx")), &roots));
        assert!(!upload_path_allowed(
            &s(&root.join("linked-folder").join("secret.xlsx")),
            &roots
        ));
        // not there yet, but below a link that leaves the root
        assert!(!upload_path_allowed(
            &s(&root.join("linked-folder").join("new.jpg")),
            &roots
        ));
        // a root that is itself a link to a folder: what is inside the target is inside the root
        let via_link = tmp.path().join("via-link");
        symlink(&root, &via_link).unwrap();
        assert!(upload_path_allowed(
            &s(&root.join("a.jpg")),
            &[s(&via_link)]
        ));
        assert!(upload_path_allowed(
            &s(&via_link.join("a.jpg")),
            &[s(&root)]
        ));
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_case_and_slashes_do_not_matter() {
        let (_tmp, root, _) = root_and_dirs();
        let roots = vec![s(&root)];
        let upper = s(&root.join("a.jpg")).to_uppercase();
        assert!(upload_path_allowed(&upper, &roots));
        let forward = s(&root.join("a.jpg")).replace('\\', "/");
        assert!(upload_path_allowed(&forward, &roots));
        let other = s(&root.parent().unwrap().join("other").join("secret.xlsx")).to_uppercase();
        assert!(!upload_path_allowed(&other, &roots));
    }
}
