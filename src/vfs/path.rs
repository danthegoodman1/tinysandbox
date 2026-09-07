use super::{Errno, VfsError, VfsResult};

pub(crate) const MAX_PATH_DEPTH: usize = 256;

pub(crate) fn normalize_path(path: &str) -> VfsResult<Vec<String>> {
    if path.is_empty() || !path.starts_with('/') || path.contains('\0') {
        return Err(VfsError::new(Errno::EINVAL));
    }

    let mut components = Vec::new();

    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            name => {
                if components.len() >= MAX_PATH_DEPTH {
                    return Err(VfsError::new(Errno::EINVAL));
                }
                components.push(name.to_owned());
            }
        }
    }

    Ok(components)
}

#[cfg(test)]
mod tests {
    use super::normalize_path;
    use crate::vfs::Errno;

    #[test]
    fn normalization_resolves_dots_and_collapses_slashes() {
        assert_eq!(
            normalize_path("/workspace//./src/../out").expect("path normalizes"),
            vec!["workspace", "out"]
        );
    }

    #[test]
    fn parent_components_are_contained_at_root() {
        assert_eq!(
            normalize_path("/../../workspace").expect("path stays contained"),
            vec!["workspace"]
        );
        assert_eq!(
            normalize_path("/..").expect("root parent is root"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn relative_paths_are_rejected() {
        let err = normalize_path("workspace/file").expect_err("relative paths are invalid");
        assert_eq!(err.errno(), Errno::EINVAL);
    }
}
