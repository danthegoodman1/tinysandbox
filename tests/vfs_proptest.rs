use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use tinysandbox::vfs::{InMemoryVfs, OpenMode, Vfs, VfsQuota};

const MAX_BYTES: u64 = 64;
const MAX_FILES: u64 = 8;
const MAX_FILE_SIZE: u64 = 16;

proptest! {
    #[test]
    fn random_valid_sequences_match_a_simple_model(ops in proptest::collection::vec((0u8..10, 0u8..5, 0u8..3, 0u8..24), 0..128)) {
        let vfs = InMemoryVfs::new(VfsQuota {
            max_bytes: MAX_BYTES,
            max_files: MAX_FILES,
            max_file_size: MAX_FILE_SIZE,
        });
        let mut model = Model::default();

        for (op, file, dir, value) in ops {
            let dir_path = format!("/dir-{}", dir % 2);
            let path = file_path(file, dir);

            match op {
                0 => {
                    let expected = model.mkdir(&dir_path);
                    prop_assert_eq!(vfs.mkdir(&dir_path).is_ok(), expected);
                }
                1 => {
                    let expected = model.rmdir(&dir_path);
                    prop_assert_eq!(vfs.rmdir(&dir_path).is_ok(), expected);
                }
                2 => {
                    let expected = model.open_create(&path);
                    let opened = vfs.open(&path, OpenMode::write_only().create());
                    prop_assert_eq!(opened.is_ok(), expected);
                    if let Ok(handle) = opened {
                        vfs.close(handle).expect("close created file");
                    }
                }
                3 => {
                    let data = vec![value; usize::from(value % 10)];
                    model_write(
                        &vfs,
                        &mut model,
                        &path,
                        u64::from(value % 8),
                        &data,
                        false,
                    )?;
                }
                4 => {
                    model_truncate(&vfs, &mut model, &path, u64::from(value % 20))?;
                }
                5 => {
                    let expected = model.unlink(&path);
                    prop_assert_eq!(vfs.unlink(&path).is_ok(), expected);
                }
                6 => {
                    let to = format!("/renamed-{}", file % 3);
                    let expected = model.rename_file(&path, &to);
                    prop_assert_eq!(vfs.rename(&path, &to).is_ok(), expected);
                }
                7 => {
                    assert_read_matches_model(&vfs, &model, &path)?;
                }
                8 => {
                    let readdir_path = if value.is_multiple_of(2) {
                        "/"
                    } else {
                        &dir_path
                    };
                    assert_readdir_matches_model(&vfs, &model, readdir_path)?;
                }
                _ => {
                    let data = vec![value; usize::from(value % 6)];
                    model_write(&vfs, &mut model, &path, 0, &data, true)?;
                }
            }

            assert_tree_matches_model(&vfs, &model)?;
            let stats = vfs.stats().expect("stats should be available");
            prop_assert_eq!(stats.used_bytes, model.used_bytes());
            prop_assert_eq!(stats.file_count, model.entry_count());
            prop_assert!(stats.used_bytes <= MAX_BYTES);
            prop_assert!(stats.file_count <= MAX_FILES);
        }
    }
}

#[derive(Debug)]
struct Model {
    dirs: BTreeSet<String>,
    files: BTreeMap<String, Vec<u8>>,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            dirs: BTreeSet::from(["/".to_owned()]),
            files: BTreeMap::new(),
        }
    }
}

impl Model {
    fn mkdir(&mut self, path: &str) -> bool {
        if self.dirs.contains(path)
            || self.files.contains_key(path)
            || !self.dirs.contains(&parent_path(path))
            || self.entry_count() >= MAX_FILES
        {
            return false;
        }

        self.dirs.insert(path.to_owned());
        true
    }

    fn rmdir(&mut self, path: &str) -> bool {
        if path == "/" || !self.dirs.contains(path) || self.has_children(path) {
            return false;
        }

        self.dirs.remove(path);
        true
    }

    fn open_create(&mut self, path: &str) -> bool {
        if self.files.contains_key(path) {
            return true;
        }

        if self.dirs.contains(path)
            || !self.dirs.contains(&parent_path(path))
            || self.entry_count() >= MAX_FILES
        {
            return false;
        }

        self.files.insert(path.to_owned(), Vec::new());
        true
    }

    fn unlink(&mut self, path: &str) -> bool {
        self.files.remove(path).is_some()
    }

    fn rename_file(&mut self, from: &str, to: &str) -> bool {
        if self.dirs.contains(to) || !self.dirs.contains(&parent_path(to)) {
            return false;
        }

        let Some(data) = self.files.remove(from) else {
            return false;
        };
        self.files.insert(to.to_owned(), data);
        true
    }

    fn can_resize(&self, path: &str, new_len: usize) -> bool {
        if u64::try_from(new_len).map_or(true, |len| len > MAX_FILE_SIZE) {
            return false;
        }

        let old_len = self.files.get(path).map_or(0, Vec::len);
        let used_bytes = self.used_bytes() - u64::try_from(old_len).expect("old length fits");
        used_bytes + u64::try_from(new_len).expect("new length fits") <= MAX_BYTES
    }

    fn used_bytes(&self) -> u64 {
        self.files
            .values()
            .map(|data| u64::try_from(data.len()).expect("file length fits"))
            .sum()
    }

    fn entry_count(&self) -> u64 {
        u64::try_from(self.files.len() + self.dirs.len() - 1).expect("entry count fits")
    }

    fn has_children(&self, path: &str) -> bool {
        self.files.keys().any(|file| parent_path(file) == path)
            || self
                .dirs
                .iter()
                .any(|dir| dir != path && parent_path(dir) == path)
    }

    fn entry_names(&self, path: &str) -> BTreeSet<String> {
        self.files
            .keys()
            .filter(|file| parent_path(file) == path)
            .chain(self.dirs.iter().filter(|dir| {
                dir.as_str() != "/" && dir.as_str() != path && parent_path(dir) == path
            }))
            .map(|entry| basename(entry).to_owned())
            .collect()
    }
}

fn model_write(
    vfs: &InMemoryVfs,
    model: &mut Model,
    path: &str,
    offset: u64,
    data: &[u8],
    append: bool,
) -> Result<(), TestCaseError> {
    let expected_open = model.open_create(path);
    let mode = if append {
        OpenMode::write_only().create().append()
    } else {
        OpenMode::read_write().create()
    };
    let opened = vfs.open(path, mode);
    prop_assert_eq!(opened.is_ok(), expected_open);

    let Ok(handle) = opened else {
        return Ok(());
    };

    let old_len = model.files.get(path).map_or(0, Vec::len);
    let write_offset = if append {
        old_len
    } else {
        usize::try_from(offset).expect("small generated offset fits")
    };
    let write_end = write_offset + data.len();
    let new_len = if data.is_empty() {
        old_len
    } else {
        old_len.max(write_end)
    };
    let expected_write = model.can_resize(path, new_len);
    prop_assert_eq!(vfs.write_at(handle, offset, data).is_ok(), expected_write);

    if expected_write {
        let file = model.files.get_mut(path).expect("model file exists");
        file.resize(new_len, 0);
        if !data.is_empty() {
            file[write_offset..write_end].copy_from_slice(data);
        }

        if !append {
            let mut buf = vec![0; file.len() + 1];
            let read = vfs
                .read_at(handle, 0, &mut buf)
                .expect("read-write handle can read");
            prop_assert_eq!(read, file.len());
            prop_assert_eq!(&buf[..read], file.as_slice());
        }
    }

    vfs.close(handle).expect("close write handle");
    Ok(())
}

fn model_truncate(
    vfs: &InMemoryVfs,
    model: &mut Model,
    path: &str,
    len: u64,
) -> Result<(), TestCaseError> {
    let expected_open = model.open_create(path);
    let opened = vfs.open(path, OpenMode::write_only().create());
    prop_assert_eq!(opened.is_ok(), expected_open);

    let Ok(handle) = opened else {
        return Ok(());
    };

    let new_len = usize::try_from(len).expect("small generated length fits");
    let expected_truncate = model.can_resize(path, new_len);
    prop_assert_eq!(vfs.truncate(handle, len).is_ok(), expected_truncate);
    if expected_truncate {
        model
            .files
            .get_mut(path)
            .expect("model file exists")
            .resize(new_len, 0);
    }

    vfs.close(handle).expect("close truncate handle");
    Ok(())
}

fn assert_read_matches_model(
    vfs: &InMemoryVfs,
    model: &Model,
    path: &str,
) -> Result<(), TestCaseError> {
    let opened = vfs.open(path, OpenMode::read_only());
    let Some(expected) = model.files.get(path) else {
        prop_assert!(opened.is_err());
        return Ok(());
    };

    let handle = opened.expect("model file opens");
    let mut buf = vec![0; expected.len() + 8];
    let read = vfs.read_at(handle, 0, &mut buf).expect("read succeeds");
    prop_assert_eq!(read, expected.len());
    prop_assert_eq!(&buf[..read], expected.as_slice());
    vfs.close(handle).expect("close read handle");
    Ok(())
}

fn assert_readdir_matches_model(
    vfs: &InMemoryVfs,
    model: &Model,
    path: &str,
) -> Result<(), TestCaseError> {
    let entries = vfs.readdir(path);
    if !model.dirs.contains(path) {
        prop_assert!(entries.is_err());
        return Ok(());
    }

    let names = entries
        .expect("model directory reads")
        .into_iter()
        .map(|entry| entry.name)
        .collect::<BTreeSet<_>>();
    prop_assert_eq!(names, model.entry_names(path));
    Ok(())
}

fn assert_tree_matches_model(vfs: &InMemoryVfs, model: &Model) -> Result<(), TestCaseError> {
    for (path, expected) in &model.files {
        assert_read_matches_model(vfs, model, path)?;
        let stat = vfs.stat(path).expect("model file stats");
        prop_assert!(stat.is_file());
        prop_assert_eq!(
            stat.len,
            u64::try_from(expected.len()).expect("length fits")
        );
    }

    for dir in &model.dirs {
        let stat = vfs.stat(dir).expect("model dir stats");
        prop_assert!(stat.is_dir());
        assert_readdir_matches_model(vfs, model, dir)?;
    }

    Ok(())
}

fn file_path(file: u8, dir: u8) -> String {
    if file.is_multiple_of(2) {
        format!("/file-{file}")
    } else {
        format!("/dir-{}/file-{file}", dir % 2)
    }
}

fn parent_path(path: &str) -> String {
    let index = path.rfind('/').expect("absolute generated path");
    if index == 0 {
        "/".to_owned()
    } else {
        path[..index].to_owned()
    }
}

fn basename(path: &str) -> &str {
    let index = path.rfind('/').expect("absolute generated path");
    &path[index + 1..]
}
