//! S3-backed virtual filesystem.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};

use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use tokio::io::{AsyncRead, AsyncReadExt};

use super::path::normalize_path;
use super::{DirEntry, Errno, FileHandle, FileType, Metadata, OpenMode, Vfs, VfsError, VfsResult};

/// Default ceiling on bytes staged in memory to modify an existing object.
const DEFAULT_MAX_EDIT_BYTES: u64 = 32 * 1024 * 1024;

/// Bytes a forward-only write buffers before it flushes a multipart part.
const INITIAL_PART_SIZE: usize = 8 * 1024 * 1024;

/// Parts uploaded at one size before the next size class doubles it. Keeps a
/// stream of unknown length inside the 10,000-part upload limit.
const PARTS_PER_SIZE_CLASS: usize = 1_000;

/// Write policy and staging limits for [`S3Vfs`].
///
/// Construct with struct update syntax so later releases can add fields:
///
/// ```
/// # use tinysandbox::vfs::S3VfsConfig;
/// let config = S3VfsConfig {
///     max_edit_bytes: 8 * 1024 * 1024,
///     ..S3VfsConfig::default()
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3VfsConfig {
    /// Rejects every write and path mutation with `EACCES`.
    ///
    /// Credentials remain the enforcing boundary; this only stops the VFS from
    /// issuing mutating requests with a client that would accept them.
    pub read_only: bool,
    /// Ceiling on bytes staged in memory to modify an existing object.
    ///
    /// Modifying an object requires reading it, applying the writes, and
    /// putting it back, so its whole body is held in memory. Objects longer
    /// than this fail with `EFBIG` and must be rewritten instead. Zero removes
    /// the limit. Forward-only writes that replace an object stream through a
    /// multipart upload and ignore this limit.
    pub max_edit_bytes: u64,
    /// Allows renaming a directory by copying and deleting every key beneath
    /// it. S3 has no atomic directory rename; see [`S3Vfs::rename`]. When
    /// false, renaming a directory fails with `EXDEV`.
    pub directory_rename: bool,
    /// Guards writes with `If-Match` and `If-None-Match` preconditions so a
    /// concurrent replacement fails instead of being silently overwritten.
    ///
    /// Disable this only for an S3-compatible service that rejects conditional
    /// writes; exclusive creation and lost-update detection go with it.
    pub conditional_writes: bool,
}

impl Default for S3VfsConfig {
    fn default() -> Self {
        Self {
            read_only: false,
            max_edit_bytes: DEFAULT_MAX_EDIT_BYTES,
            directory_rename: true,
            conditional_writes: true,
        }
    }
}

impl S3VfsConfig {
    /// Returns a configuration that rejects every mutation with `EACCES`.
    pub fn read_only() -> Self {
        Self {
            read_only: true,
            ..Self::default()
        }
    }
}

/// A read-write view of one S3 bucket prefix.
///
/// The supplied client owns endpoint, credentials, retry, timeout, TLS, region,
/// and path-style policy. Construction performs no network I/O.
///
/// S3 has no partial-object update, so a writable handle stages its contents
/// and lands them as one object operation when the handle closes. Writes become
/// visible to other handles and other readers at that point, not before.
pub struct S3Vfs {
    ops: Arc<dyn S3Ops>,
    bucket: String,
    root_prefix: String,
    config: S3VfsConfig,
    part_size: usize,
    state: Mutex<State>,
}

impl std::fmt::Debug for S3Vfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Vfs")
            .field("bucket", &self.bucket)
            .field("root_prefix", &self.root_prefix)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl S3Vfs {
    /// Creates a read-write filesystem rooted at `prefix` in `bucket`.
    ///
    /// Leading and trailing slashes in the prefix are normalized. Empty,
    /// current-directory, parent-directory, and NUL-containing components are
    /// rejected. The empty prefix exposes the whole bucket.
    ///
    /// Use [`S3Vfs::with_config`] and [`S3VfsConfig::read_only`] for a view
    /// that refuses every mutation.
    pub fn new(
        client: aws_sdk_s3::Client,
        bucket: impl Into<String>,
        prefix: Option<&str>,
    ) -> VfsResult<Self> {
        Self::with_config(client, bucket, prefix, S3VfsConfig::default())
    }

    /// Creates a filesystem rooted at `prefix` in `bucket` with `config`.
    pub fn with_config(
        client: aws_sdk_s3::Client,
        bucket: impl Into<String>,
        prefix: Option<&str>,
        config: S3VfsConfig,
    ) -> VfsResult<Self> {
        Self::with_ops(Arc::new(AwsS3Ops { client }), bucket.into(), prefix, config)
    }

    fn with_ops(
        ops: Arc<dyn S3Ops>,
        bucket: String,
        prefix: Option<&str>,
        config: S3VfsConfig,
    ) -> VfsResult<Self> {
        validate_bucket(&bucket)?;
        let root_prefix = normalize_prefix(prefix.unwrap_or_default())?;
        Ok(Self {
            ops,
            bucket,
            root_prefix,
            config,
            part_size: INITIAL_PART_SIZE,
            state: Mutex::new(State {
                next_handle: 1,
                handles: BTreeMap::new(),
            }),
        })
    }

    /// Bytes buffered before the next part uploads. The size class doubles
    /// every [`PARTS_PER_SIZE_CLASS`] parts so an object of unknown length
    /// stays inside the 10,000-part upload limit.
    fn part_size(&self, uploaded_parts: usize) -> usize {
        self.part_size
            .saturating_mul(1 << (uploaded_parts / PARTS_PER_SIZE_CLASS).min(16))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn components(&self, path: &str) -> VfsResult<Vec<String>> {
        normalize_path(path)
    }

    fn key(&self, components: &[String]) -> String {
        if components.is_empty() {
            return self.root_prefix.trim_end_matches('/').to_owned();
        }
        format!("{}{}", self.root_prefix, components.join("/"))
    }

    fn directory_prefix(&self, components: &[String]) -> String {
        if components.is_empty() {
            return self.root_prefix.clone();
        }
        format!("{}/", self.key(components))
    }

    fn require_writable(&self) -> VfsResult<()> {
        if self.config.read_only {
            return Err(vfs_error(Errno::EACCES));
        }
        Ok(())
    }

    fn validate_parents(&self, components: &[String]) -> VfsResult<()> {
        for end in 1..components.len() {
            match self.kind_at(&components[..end])? {
                Some(Kind::Directory(_)) => {}
                Some(Kind::File(_)) => return Err(vfs_error(Errno::ENOTDIR)),
                None => return Err(vfs_error(Errno::ENOENT)),
            }
        }
        Ok(())
    }

    fn kind_at(&self, components: &[String]) -> VfsResult<Option<Kind>> {
        if components.is_empty() {
            return Ok(Some(Kind::Directory(Vec::new())));
        }

        let listing = self.list_directory(components)?;
        if listing.exists {
            return Ok(Some(Kind::Directory(listing.entries)));
        }

        match self.ops.head(HeadRequest {
            bucket: self.bucket.clone(),
            key: self.key(components),
        }) {
            Ok(file) => Ok(Some(Kind::File(file))),
            Err(RemoteError::Missing) => Ok(None),
            Err(err) => Err(err.into_vfs()),
        }
    }

    fn list_directory(&self, components: &[String]) -> VfsResult<DirectoryListing> {
        let prefix = self.directory_prefix(components);
        let mut continuation = None;
        let mut seen_tokens = BTreeSet::new();
        let mut entries: BTreeMap<String, Metadata> = BTreeMap::new();
        let mut exists = components.is_empty();

        loop {
            let page = self
                .ops
                .list(ListRequest {
                    bucket: self.bucket.clone(),
                    prefix: prefix.clone(),
                    delimiter: "/".to_owned(),
                    continuation: continuation.clone(),
                    max_keys: None,
                })
                .map_err(RemoteError::into_vfs)?;

            for object in page.objects {
                let Some(relative) = object.key.strip_prefix(&prefix) else {
                    continue;
                };
                if relative.is_empty() {
                    if object.size == 0 {
                        exists = true;
                    }
                    continue;
                }
                exists = true;
                if valid_entry_name(relative) {
                    entries.entry(relative.to_owned()).or_insert(Metadata {
                        file_type: FileType::File,
                        len: object.size,
                    });
                }
            }

            for common_prefix in page.common_prefixes {
                let Some(relative) = common_prefix.strip_prefix(&prefix) else {
                    continue;
                };
                let Some(name) = relative.strip_suffix('/') else {
                    continue;
                };
                exists = true;
                if valid_entry_name(name) {
                    // Directory identity wins over a colliding object.
                    entries.insert(
                        name.to_owned(),
                        Metadata {
                            file_type: FileType::Directory,
                            len: 0,
                        },
                    );
                }
            }

            if !page.truncated {
                break;
            }
            let Some(next) = page.next_continuation.filter(|token| !token.is_empty()) else {
                return Err(vfs_error(Errno::EIO));
            };
            if !seen_tokens.insert(next.clone()) {
                return Err(vfs_error(Errno::EIO));
            }
            continuation = Some(next);
        }

        Ok(DirectoryListing {
            exists,
            entries: entries
                .into_iter()
                .map(|(name, metadata)| DirEntry { name, metadata })
                .collect(),
        })
    }

    /// Lists every key beneath `prefix`, including the prefix marker itself.
    fn list_recursive(&self, prefix: &str) -> VfsResult<Vec<String>> {
        let mut continuation = None;
        let mut seen_tokens = BTreeSet::new();
        let mut keys = Vec::new();

        loop {
            let page = self
                .ops
                .list(ListRequest {
                    bucket: self.bucket.clone(),
                    prefix: prefix.to_owned(),
                    delimiter: String::new(),
                    continuation: continuation.clone(),
                    max_keys: None,
                })
                .map_err(RemoteError::into_vfs)?;

            for object in page.objects {
                if object.key.starts_with(prefix) {
                    keys.push(object.key);
                }
            }

            if !page.truncated {
                break;
            }
            let Some(next) = page.next_continuation.filter(|token| !token.is_empty()) else {
                return Err(vfs_error(Errno::EIO));
            };
            if !seen_tokens.insert(next.clone()) {
                return Err(vfs_error(Errno::EIO));
            }
            continuation = Some(next);
        }

        Ok(keys)
    }

    /// Recreates a directory marker when a removal leaves the directory with no
    /// keys, so a directory does not disappear along with its last child.
    fn preserve_directory(&self, components: &[String]) -> VfsResult<()> {
        if components.is_empty() {
            return Ok(());
        }

        let prefix = self.directory_prefix(components);
        let page = self
            .ops
            .list(ListRequest {
                bucket: self.bucket.clone(),
                prefix: prefix.clone(),
                delimiter: "/".to_owned(),
                continuation: None,
                max_keys: Some(1),
            })
            .map_err(RemoteError::into_vfs)?;
        if !page.objects.is_empty() || !page.common_prefixes.is_empty() {
            return Ok(());
        }

        self.put_object(prefix, Vec::new(), Precondition::None)
    }

    fn precondition(&self, precondition: Precondition) -> Precondition {
        if self.config.conditional_writes {
            precondition
        } else {
            Precondition::None
        }
    }

    fn put_object(&self, key: String, body: Vec<u8>, precondition: Precondition) -> VfsResult<()> {
        let exclusive = precondition == Precondition::Absent;
        self.ops
            .put(PutRequest {
                bucket: self.bucket.clone(),
                key,
                body,
                precondition: self.precondition(precondition),
            })
            .map_err(|err| exclusive_error(err, exclusive))
    }

    fn check_edit_len(&self, len: u64) -> VfsResult<()> {
        if self.config.max_edit_bytes != 0 && len > self.config.max_edit_bytes {
            return Err(vfs_error(Errno::EFBIG));
        }
        Ok(())
    }

    /// Downloads a whole object at the revision the handle pinned.
    fn get_whole(&self, key: &str, file: &RemoteFile) -> VfsResult<Vec<u8>> {
        if file.len == 0 {
            return Ok(Vec::new());
        }
        let end = file.len - 1;
        let expected_content_range = format!("bytes 0-{end}/{}", file.len);
        let response = self
            .ops
            .get(GetRequest {
                bucket: self.bucket.clone(),
                key: key.to_owned(),
                range: format!("bytes=0-{end}"),
                if_match: file.etag.clone(),
                expected_len: file.len,
                expected_content_range: expected_content_range.clone(),
            })
            .map_err(RemoteError::into_vfs)?;

        let expected = usize::try_from(file.len).map_err(|_| vfs_error(Errno::EFBIG))?;
        if response.body.len() != expected
            || response.content_length != file.len
            || response.etag.as_deref() != Some(file.etag.as_str())
            || response.content_range.as_deref() != Some(expected_content_range.as_str())
        {
            return Err(vfs_error(Errno::EIO));
        }
        Ok(response.body)
    }

    fn handle_state(&self, handle: FileHandle) -> VfsResult<Arc<Mutex<HandleState>>> {
        self.state()
            .handles
            .get(&handle)
            .map(Arc::clone)
            .ok_or(vfs_error(Errno::EBADF))
    }

    fn insert_handle(
        &self,
        key: String,
        mode: OpenMode,
        staging: Staging,
    ) -> VfsResult<FileHandle> {
        let mut state = self.state();
        let handle = FileHandle::new(state.next_handle);
        state.next_handle = state
            .next_handle
            .checked_add(1)
            .ok_or(vfs_error(Errno::EIO))?;
        state.handles.insert(
            handle,
            Arc::new(Mutex::new(HandleState { key, mode, staging })),
        );
        Ok(handle)
    }

    /// Materializes a staged body, downloading the pinned revision on first use.
    fn materialize<'a>(&self, key: &str, buffer: &'a mut Buffer) -> VfsResult<&'a mut Vec<u8>> {
        if buffer.bytes.is_none() {
            let bytes = match &buffer.source {
                Some(file) => {
                    self.check_edit_len(file.len)?;
                    self.get_whole(key, file)?
                }
                None => Vec::new(),
            };
            buffer.bytes = Some(bytes);
        }
        buffer.bytes.as_mut().ok_or(vfs_error(Errno::EIO))
    }

    fn resize_staged(&self, bytes: &mut Vec<u8>, len: usize) -> VfsResult<()> {
        self.check_edit_len(u64::try_from(len).map_err(|_| vfs_error(Errno::EFBIG))?)?;
        bytes.resize(len, 0);
        Ok(())
    }

    /// Uploads whole parts from the pending tail, growing the part size so an
    /// object of unknown length stays inside the 10,000-part limit.
    fn flush_parts(&self, key: &str, stream: &mut Stream) -> VfsResult<()> {
        loop {
            let uploaded = stream
                .upload
                .as_ref()
                .map_or(0, |upload| upload.parts.len());
            let part_size = self.part_size(uploaded);
            if stream.buffer.len() < part_size {
                return Ok(());
            }

            let upload_id = match &stream.upload {
                Some(upload) => upload.id.clone(),
                None => {
                    let id = self
                        .ops
                        .create_upload(CreateUploadRequest {
                            bucket: self.bucket.clone(),
                            key: key.to_owned(),
                        })
                        .map_err(RemoteError::into_vfs)?;
                    stream.upload = Some(Upload {
                        id: id.clone(),
                        parts: Vec::new(),
                    });
                    id
                }
            };

            let body: Vec<u8> = stream.buffer.drain(..part_size).collect();
            let uploaded_len = u64::try_from(body.len()).map_err(|_| vfs_error(Errno::EIO))?;
            let part_number = i32::try_from(uploaded + 1).map_err(|_| vfs_error(Errno::EFBIG))?;
            let etag = self
                .ops
                .upload_part(UploadPartRequest {
                    bucket: self.bucket.clone(),
                    key: key.to_owned(),
                    upload_id,
                    part_number,
                    body,
                })
                .map_err(RemoteError::into_vfs)?;

            let upload = stream.upload.as_mut().ok_or(vfs_error(Errno::EIO))?;
            upload.parts.push(UploadedPart { part_number, etag });
            stream.flushed = stream
                .flushed
                .checked_add(uploaded_len)
                .ok_or(vfs_error(Errno::EFBIG))?;
        }
    }

    /// Extends a stream with zeros up to `target`, flushing as it goes so a
    /// sparse write never allocates the whole gap.
    fn fill_gap(&self, key: &str, stream: &mut Stream, target: u64) -> VfsResult<()> {
        loop {
            let end = stream.end()?;
            if end >= target {
                return Ok(());
            }
            let uploaded = stream
                .upload
                .as_ref()
                .map_or(0, |upload| upload.parts.len());
            let room = self
                .part_size(uploaded)
                .saturating_sub(stream.buffer.len())
                .max(1);
            let gap = usize::try_from(target - end).unwrap_or(usize::MAX);
            stream.buffer.resize(stream.buffer.len() + room.min(gap), 0);
            self.flush_parts(key, stream)?;
        }
    }

    fn abort_upload(&self, key: &str, upload: &Upload) {
        let _ = self.ops.abort_upload(AbortUploadRequest {
            bucket: self.bucket.clone(),
            key: key.to_owned(),
            upload_id: upload.id.clone(),
        });
    }

    /// Lands a staged handle in S3. Aborts any multipart upload on failure so a
    /// failed write leaves no billable parts behind.
    fn commit(&self, state: &mut HandleState) -> VfsResult<()> {
        match &mut state.staging {
            Staging::Remote(_) => Ok(()),
            Staging::Buffer(buffer) => {
                if !buffer.dirty {
                    return Ok(());
                }
                let bytes = buffer.bytes.take().unwrap_or_default();
                self.put_object(state.key.clone(), bytes, buffer.precondition.clone())
            }
            Staging::Stream(stream) => {
                let result = self.finish_stream(&state.key, stream);
                if result.is_err()
                    && let Some(upload) = &stream.upload
                {
                    self.abort_upload(&state.key, upload);
                }
                result
            }
        }
    }

    fn finish_stream(&self, key: &str, stream: &mut Stream) -> VfsResult<()> {
        let Some(upload) = &stream.upload else {
            let body = std::mem::take(&mut stream.buffer);
            return self.put_object(key.to_owned(), body, stream.precondition.clone());
        };

        let mut parts = upload.parts.clone();
        if !stream.buffer.is_empty() || parts.is_empty() {
            let part_number =
                i32::try_from(parts.len() + 1).map_err(|_| vfs_error(Errno::EFBIG))?;
            let etag = self
                .ops
                .upload_part(UploadPartRequest {
                    bucket: self.bucket.clone(),
                    key: key.to_owned(),
                    upload_id: upload.id.clone(),
                    part_number,
                    body: std::mem::take(&mut stream.buffer),
                })
                .map_err(RemoteError::into_vfs)?;
            parts.push(UploadedPart { part_number, etag });
        }

        let exclusive = stream.precondition == Precondition::Absent;
        self.ops
            .complete_upload(CompleteUploadRequest {
                bucket: self.bucket.clone(),
                key: key.to_owned(),
                upload_id: upload.id.clone(),
                parts,
                precondition: self.precondition(stream.precondition.clone()),
            })
            .map_err(|err| exclusive_error(err, exclusive))
    }

    fn rename_file(&self, from: &[String], to: &[String], file: &RemoteFile) -> VfsResult<()> {
        let from_key = self.key(from);
        let to_key = self.key(to);
        self.ops
            .copy(CopyRequest {
                bucket: self.bucket.clone(),
                source_key: from_key.clone(),
                key: to_key,
                source_if_match: self.precondition(Precondition::Match(file.etag.clone())),
            })
            .map_err(RemoteError::into_vfs)?;
        self.ops
            .delete(DeleteRequest {
                bucket: self.bucket.clone(),
                key: from_key,
                precondition: self.precondition(Precondition::Match(file.etag.clone())),
            })
            .map_err(RemoteError::into_vfs)
    }

    /// Renames a directory by copying every key beneath it and then deleting
    /// the originals. S3 has no atomic directory rename, so a failure part way
    /// through leaves keys under both prefixes.
    fn rename_directory(&self, from: &[String], to: &[String]) -> VfsResult<()> {
        let from_prefix = self.directory_prefix(from);
        let to_prefix = self.directory_prefix(to);
        let keys = self.list_recursive(&from_prefix)?;

        for key in &keys {
            let Some(suffix) = key.strip_prefix(&from_prefix) else {
                continue;
            };
            self.ops
                .copy(CopyRequest {
                    bucket: self.bucket.clone(),
                    source_key: key.clone(),
                    key: format!("{to_prefix}{suffix}"),
                    source_if_match: Precondition::None,
                })
                .map_err(RemoteError::into_vfs)?;
        }

        for key in keys {
            self.ops
                .delete(DeleteRequest {
                    bucket: self.bucket.clone(),
                    key,
                    precondition: Precondition::None,
                })
                .map_err(RemoteError::into_vfs)?;
        }
        Ok(())
    }
}

impl Drop for S3Vfs {
    fn drop(&mut self) {
        let handles = std::mem::take(&mut self.state().handles);
        for handle in handles.into_values() {
            let state = handle.lock().unwrap_or_else(PoisonError::into_inner);
            if let Staging::Stream(stream) = &state.staging
                && let Some(upload) = &stream.upload
            {
                self.abort_upload(&state.key, upload);
            }
        }
    }
}

impl Vfs for S3Vfs {
    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        let components = self.components(path)?;
        self.validate_parents(&components)?;
        match self.kind_at(&components)? {
            Some(Kind::Directory(_)) => Ok(Metadata {
                file_type: FileType::Directory,
                len: 0,
            }),
            Some(Kind::File(file)) => Ok(Metadata {
                file_type: FileType::File,
                len: file.len,
            }),
            None => Err(vfs_error(Errno::ENOENT)),
        }
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let components = self.components(path)?;
        self.validate_parents(&components)?;
        if components.is_empty() {
            return Ok(self.list_directory(&components)?.entries);
        }
        match self.kind_at(&components)? {
            Some(Kind::Directory(entries)) => Ok(entries),
            Some(Kind::File(_)) => Err(vfs_error(Errno::ENOTDIR)),
            None => Err(vfs_error(Errno::ENOENT)),
        }
    }

    fn mkdir(&self, path: &str) -> VfsResult<()> {
        let components = self.components(path)?;
        self.require_writable()?;
        if components.is_empty() {
            return Err(vfs_error(Errno::EEXIST));
        }
        self.validate_parents(&components)?;
        if self.kind_at(&components)?.is_some() {
            return Err(vfs_error(Errno::EEXIST));
        }
        self.put_object(
            self.directory_prefix(&components),
            Vec::new(),
            Precondition::Absent,
        )
    }

    /// Renames a file or directory.
    ///
    /// A file is copied to the new key and the old key deleted. A directory is
    /// renamed by copying and deleting every key beneath it, which is neither
    /// atomic nor cheap: it costs two requests per key, and an interrupted
    /// rename leaves keys under both prefixes. Set
    /// [`S3VfsConfig::directory_rename`] to false to reject it with `EXDEV`.
    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        let from_components = self.components(from)?;
        let to_components = self.components(to)?;
        self.require_writable()?;
        if from_components.is_empty() || to_components.is_empty() {
            return Err(vfs_error(Errno::EINVAL));
        }

        self.validate_parents(&from_components)?;
        let source = self
            .kind_at(&from_components)?
            .ok_or(vfs_error(Errno::ENOENT))?;
        if from_components == to_components {
            return Ok(());
        }

        if matches!(source, Kind::Directory(_)) && to_components.starts_with(&from_components) {
            return Err(vfs_error(Errno::EINVAL));
        }

        self.validate_parents(&to_components)?;
        match (&source, self.kind_at(&to_components)?) {
            (Kind::File(_), Some(Kind::Directory(_))) => return Err(vfs_error(Errno::EISDIR)),
            (Kind::Directory(_), Some(Kind::File(_))) => return Err(vfs_error(Errno::ENOTDIR)),
            (Kind::Directory(_), Some(Kind::Directory(entries))) if !entries.is_empty() => {
                return Err(vfs_error(Errno::ENOTEMPTY));
            }
            _ => {}
        }

        match &source {
            Kind::File(file) => self.rename_file(&from_components, &to_components, file)?,
            Kind::Directory(_) => {
                if !self.config.directory_rename {
                    return Err(vfs_error(Errno::EXDEV));
                }
                self.rename_directory(&from_components, &to_components)?;
            }
        }

        self.preserve_directory(&from_components[..from_components.len() - 1])
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        let components = self.components(path)?;
        self.require_writable()?;
        if components.is_empty() {
            return Err(vfs_error(Errno::EISDIR));
        }
        self.validate_parents(&components)?;
        let file = match self.kind_at(&components)? {
            Some(Kind::Directory(_)) => return Err(vfs_error(Errno::EISDIR)),
            Some(Kind::File(file)) => file,
            None => return Err(vfs_error(Errno::ENOENT)),
        };

        self.ops
            .delete(DeleteRequest {
                bucket: self.bucket.clone(),
                key: self.key(&components),
                precondition: self.precondition(Precondition::Match(file.etag)),
            })
            .map_err(RemoteError::into_vfs)?;
        self.preserve_directory(&components[..components.len() - 1])
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        let components = self.components(path)?;
        self.require_writable()?;
        if components.is_empty() {
            return Err(vfs_error(Errno::EBUSY));
        }
        self.validate_parents(&components)?;
        match self.kind_at(&components)? {
            Some(Kind::File(_)) => return Err(vfs_error(Errno::ENOTDIR)),
            Some(Kind::Directory(entries)) if !entries.is_empty() => {
                return Err(vfs_error(Errno::ENOTEMPTY));
            }
            Some(Kind::Directory(_)) => {}
            None => return Err(vfs_error(Errno::ENOENT)),
        }

        match self.ops.delete(DeleteRequest {
            bucket: self.bucket.clone(),
            key: self.directory_prefix(&components),
            precondition: Precondition::None,
        }) {
            Ok(()) | Err(RemoteError::Missing) => {}
            Err(err) => return Err(err.into_vfs()),
        }
        self.preserve_directory(&components[..components.len() - 1])
    }

    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        mode.validate()?;
        let components = self.components(path)?;
        if mode.write || mode.create {
            // Refused before any request so a read-only mount costs nothing.
            // A creating mode mutates the bucket even without write access.
            self.require_writable()?;
        }
        if components.is_empty() {
            return Err(vfs_error(Errno::EISDIR));
        }
        self.validate_parents(&components)?;

        let existing = match self.kind_at(&components)? {
            Some(Kind::Directory(_)) => return Err(vfs_error(Errno::EISDIR)),
            Some(Kind::File(file)) => Some(file),
            None => None,
        };
        let key = self.key(&components);

        if !mode.write {
            return match existing {
                Some(file) => {
                    if mode.create_new {
                        return Err(vfs_error(Errno::EEXIST));
                    }
                    self.insert_handle(key, mode, Staging::Remote(file))
                }
                // A read handle that also creates lands an empty object on
                // close; its writes still fail with `EBADF`.
                None if mode.create => self.insert_handle(
                    key,
                    mode,
                    Staging::Buffer(Buffer {
                        source: None,
                        precondition: creation_precondition(mode),
                        bytes: Some(Vec::new()),
                        dirty: true,
                    }),
                ),
                None => Err(vfs_error(Errno::ENOENT)),
            };
        }

        let staging = match existing {
            Some(file) => {
                if mode.create_new {
                    return Err(vfs_error(Errno::EEXIST));
                }
                let precondition = Precondition::Match(file.etag.clone());
                if mode.truncate && !mode.read {
                    Staging::Stream(Stream::new(precondition))
                } else if mode.truncate {
                    Staging::Buffer(Buffer {
                        source: Some(file),
                        precondition,
                        bytes: Some(Vec::new()),
                        dirty: true,
                    })
                } else {
                    // Left unmaterialized so `touch` on an existing object and
                    // an unwritten read-write open cost no download.
                    Staging::Buffer(Buffer {
                        source: Some(file),
                        precondition,
                        bytes: None,
                        dirty: false,
                    })
                }
            }
            None => {
                if !mode.create {
                    return Err(vfs_error(Errno::ENOENT));
                }
                let precondition = creation_precondition(mode);
                if mode.read {
                    Staging::Buffer(Buffer {
                        source: None,
                        precondition,
                        bytes: Some(Vec::new()),
                        dirty: true,
                    })
                } else {
                    Staging::Stream(Stream::new(precondition))
                }
            }
        };
        self.insert_handle(key, mode, staging)
    }

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let shared = self.handle_state(handle)?;
        let mut state = shared.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.mode.read {
            return Err(vfs_error(Errno::EBADF));
        }

        let (key, remote) = match &state.staging {
            Staging::Remote(file) => (state.key.clone(), Some(file.clone())),
            Staging::Buffer(_) => (state.key.clone(), None),
            Staging::Stream(_) => return Err(vfs_error(Errno::EBADF)),
        };

        if let Some(file) = remote {
            // Ranged reads do not touch handle state, so the lock is released
            // before the request.
            drop(state);
            return self.read_remote(&key, &file, offset, buf);
        }

        let Staging::Buffer(buffer) = &mut state.staging else {
            return Err(vfs_error(Errno::EBADF));
        };
        let bytes = self.materialize(&key, buffer)?;
        let offset = usize::try_from(offset).map_err(|_| vfs_error(Errno::EINVAL))?;
        if buf.is_empty() || offset >= bytes.len() {
            return Ok(0);
        }
        let len = (bytes.len() - offset).min(buf.len());
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        let shared = self.handle_state(handle)?;
        let mut state = shared.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.mode.write {
            return Err(vfs_error(Errno::EBADF));
        }
        if data.is_empty() {
            return Ok(0);
        }

        let key = state.key.clone();
        let append = state.mode.append;
        match &mut state.staging {
            Staging::Remote(_) => Err(vfs_error(Errno::EBADF)),
            Staging::Buffer(buffer) => {
                let bytes = self.materialize(&key, buffer)?;
                let start = if append {
                    bytes.len()
                } else {
                    usize::try_from(offset).map_err(|_| vfs_error(Errno::EINVAL))?
                };
                let end = start
                    .checked_add(data.len())
                    .ok_or(vfs_error(Errno::EINVAL))?;
                if end > bytes.len() {
                    self.resize_staged(bytes, end)?;
                }
                bytes[start..end].copy_from_slice(data);
                buffer.dirty = true;
                Ok(data.len())
            }
            Staging::Stream(stream) => {
                let target = if append { stream.end()? } else { offset };
                if target < stream.flushed {
                    // The bytes being overwritten are already committed to an
                    // uploaded part and can no longer be staged in memory.
                    return Err(vfs_error(Errno::EFBIG));
                }
                self.fill_gap(&key, stream, target)?;
                let start = usize::try_from(target - stream.flushed)
                    .map_err(|_| vfs_error(Errno::EINVAL))?;
                let end = start
                    .checked_add(data.len())
                    .ok_or(vfs_error(Errno::EINVAL))?;
                if end > stream.buffer.len() {
                    stream.buffer.resize(end, 0);
                }
                stream.buffer[start..end].copy_from_slice(data);
                self.flush_parts(&key, stream)?;
                Ok(data.len())
            }
        }
    }

    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        let shared = self.handle_state(handle)?;
        let mut state = shared.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.mode.write {
            return Err(vfs_error(Errno::EINVAL));
        }

        let key = state.key.clone();
        match &mut state.staging {
            Staging::Remote(_) => Err(vfs_error(Errno::EINVAL)),
            Staging::Buffer(buffer) => {
                let bytes = self.materialize(&key, buffer)?;
                let len = usize::try_from(len).map_err(|_| vfs_error(Errno::EFBIG))?;
                self.resize_staged(bytes, len)?;
                buffer.dirty = true;
                Ok(())
            }
            Staging::Stream(stream) => {
                if len < stream.flushed {
                    return Err(vfs_error(Errno::EFBIG));
                }
                if len > stream.end()? {
                    self.fill_gap(&key, stream, len)?;
                    return Ok(());
                }
                let keep =
                    usize::try_from(len - stream.flushed).map_err(|_| vfs_error(Errno::EINVAL))?;
                stream.buffer.truncate(keep);
                Ok(())
            }
        }
    }

    fn abort(&self, handle: FileHandle) -> VfsResult<()> {
        let shared = self
            .state()
            .handles
            .remove(&handle)
            .ok_or(vfs_error(Errno::EBADF))?;
        let state = shared.lock().unwrap_or_else(PoisonError::into_inner);
        if let Staging::Stream(stream) = &state.staging
            && let Some(upload) = &stream.upload
        {
            self.ops
                .abort_upload(AbortUploadRequest {
                    bucket: self.bucket.clone(),
                    key: state.key.clone(),
                    upload_id: upload.id.clone(),
                })
                .map_err(RemoteError::into_vfs)?;
        }
        Ok(())
    }

    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        let shared = self
            .state()
            .handles
            .remove(&handle)
            .ok_or(vfs_error(Errno::EBADF))?;
        let mut state = shared.lock().unwrap_or_else(PoisonError::into_inner);
        self.commit(&mut state)
    }
}

impl S3Vfs {
    fn read_remote(
        &self,
        key: &str,
        file: &RemoteFile,
        offset: u64,
        buf: &mut [u8],
    ) -> VfsResult<usize> {
        if buf.is_empty() || offset >= file.len {
            return Ok(0);
        }

        let remaining = file.len - offset;
        let requested = remaining.min(u64::try_from(buf.len()).map_err(|_| vfs_error(Errno::EIO))?);
        let end = offset
            .checked_add(requested)
            .and_then(|value| value.checked_sub(1))
            .ok_or(vfs_error(Errno::EIO))?;
        let range = format!("bytes={offset}-{end}");
        let expected_content_range = format!("bytes {offset}-{end}/{}", file.len);
        let response = self
            .ops
            .get(GetRequest {
                bucket: self.bucket.clone(),
                key: key.to_owned(),
                range,
                if_match: file.etag.clone(),
                expected_len: requested,
                expected_content_range: expected_content_range.clone(),
            })
            .map_err(RemoteError::into_vfs)?;

        let expected = usize::try_from(requested).map_err(|_| vfs_error(Errno::EIO))?;
        if response.body.len() != expected
            || response.content_length != requested
            || response.etag.as_deref() != Some(file.etag.as_str())
            || response.content_range.as_deref() != Some(expected_content_range.as_str())
        {
            return Err(vfs_error(Errno::EIO));
        }
        buf[..expected].copy_from_slice(&response.body);
        Ok(expected)
    }
}

#[derive(Debug)]
struct State {
    next_handle: u64,
    handles: BTreeMap<FileHandle, Arc<Mutex<HandleState>>>,
}

#[derive(Debug)]
struct HandleState {
    key: String,
    mode: OpenMode,
    staging: Staging,
}

#[derive(Debug)]
enum Staging {
    /// Reads served by ranged requests against a pinned object revision.
    Remote(RemoteFile),
    /// Whole body staged in memory and written back when the handle closes.
    Buffer(Buffer),
    /// Forward-only writes flushed through a multipart upload.
    Stream(Stream),
}

#[derive(Debug)]
struct Buffer {
    /// Revision to materialize from, or `None` for a new object.
    source: Option<RemoteFile>,
    precondition: Precondition,
    /// Staged body, absent until the first read or write materializes it.
    bytes: Option<Vec<u8>>,
    dirty: bool,
}

#[derive(Debug)]
struct Stream {
    /// Bytes already committed to uploaded parts.
    flushed: u64,
    /// Pending tail below the current part size.
    buffer: Vec<u8>,
    upload: Option<Upload>,
    precondition: Precondition,
}

impl Stream {
    const fn new(precondition: Precondition) -> Self {
        Self {
            flushed: 0,
            buffer: Vec::new(),
            upload: None,
            precondition,
        }
    }

    fn end(&self) -> VfsResult<u64> {
        self.flushed
            .checked_add(u64::try_from(self.buffer.len()).map_err(|_| vfs_error(Errno::EIO))?)
            .ok_or(vfs_error(Errno::EFBIG))
    }
}

#[derive(Debug)]
struct Upload {
    id: String,
    parts: Vec<UploadedPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UploadedPart {
    part_number: i32,
    etag: String,
}

#[derive(Debug)]
enum Kind {
    Directory(Vec<DirEntry>),
    File(RemoteFile),
}

#[derive(Debug)]
struct DirectoryListing {
    exists: bool,
    entries: Vec<DirEntry>,
}

/// Precondition attached to a mutating request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Precondition {
    /// Unconditional.
    None,
    /// The key must not exist (`If-None-Match: *`).
    Absent,
    /// The key must still hold this ETag (`If-Match`).
    Match(String),
}

/// Exclusive creation is enforced when the object lands, since a staged handle
/// puts nothing in the bucket until it closes.
fn creation_precondition(mode: OpenMode) -> Precondition {
    if mode.create_new {
        Precondition::Absent
    } else {
        Precondition::None
    }
}

fn exclusive_error(err: RemoteError, exclusive: bool) -> VfsError {
    match err {
        RemoteError::Precondition if exclusive => vfs_error(Errno::EEXIST),
        other => other.into_vfs(),
    }
}

fn validate_bucket(bucket: &str) -> VfsResult<()> {
    if bucket.is_empty() || bucket.contains(['\0', '/']) || bucket.chars().any(char::is_whitespace)
    {
        return Err(vfs_error(Errno::EINVAL));
    }
    Ok(())
}

fn normalize_prefix(prefix: &str) -> VfsResult<String> {
    if prefix.contains('\0') {
        return Err(vfs_error(Errno::EINVAL));
    }
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        return Ok(String::new());
    }
    if prefix
        .split('/')
        .any(|component| matches!(component, "" | "." | ".."))
    {
        return Err(vfs_error(Errno::EINVAL));
    }
    Ok(format!("{prefix}/"))
}

fn valid_entry_name(name: &str) -> bool {
    !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\0'])
}

const fn vfs_error(errno: Errno) -> VfsError {
    VfsError::new(errno)
}

trait S3Ops: Send + Sync {
    fn head(&self, request: HeadRequest) -> Result<HeadResult, RemoteError>;
    fn list(&self, request: ListRequest) -> Result<ListResult, RemoteError>;
    fn get(&self, request: GetRequest) -> Result<GetResult, RemoteError>;
    fn put(&self, request: PutRequest) -> Result<(), RemoteError>;
    fn delete(&self, request: DeleteRequest) -> Result<(), RemoteError>;
    fn copy(&self, request: CopyRequest) -> Result<(), RemoteError>;
    fn create_upload(&self, request: CreateUploadRequest) -> Result<String, RemoteError>;
    fn upload_part(&self, request: UploadPartRequest) -> Result<String, RemoteError>;
    fn complete_upload(&self, request: CompleteUploadRequest) -> Result<(), RemoteError>;
    fn abort_upload(&self, request: AbortUploadRequest) -> Result<(), RemoteError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeadRequest {
    bucket: String,
    key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListRequest {
    bucket: String,
    prefix: String,
    /// Empty lists every key beneath the prefix instead of one level.
    delimiter: String,
    continuation: Option<String>,
    max_keys: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GetRequest {
    bucket: String,
    key: String,
    range: String,
    if_match: String,
    expected_len: u64,
    expected_content_range: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PutRequest {
    bucket: String,
    key: String,
    body: Vec<u8>,
    precondition: Precondition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeleteRequest {
    bucket: String,
    key: String,
    precondition: Precondition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyRequest {
    bucket: String,
    source_key: String,
    key: String,
    source_if_match: Precondition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreateUploadRequest {
    bucket: String,
    key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UploadPartRequest {
    bucket: String,
    key: String,
    upload_id: String,
    part_number: i32,
    body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompleteUploadRequest {
    bucket: String,
    key: String,
    upload_id: String,
    parts: Vec<UploadedPart>,
    precondition: Precondition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AbortUploadRequest {
    bucket: String,
    key: String,
    upload_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteFile {
    len: u64,
    etag: String,
}

/// Result of a `HEAD` on one key.
type HeadResult = RemoteFile;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListedObject {
    key: String,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListResult {
    objects: Vec<ListedObject>,
    common_prefixes: Vec<String>,
    truncated: bool,
    next_continuation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GetResult {
    body: Vec<u8>,
    content_length: u64,
    etag: Option<String>,
    content_range: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteError {
    Missing,
    Denied,
    Precondition,
    Io,
}

impl RemoteError {
    const fn into_vfs(self) -> VfsError {
        vfs_error(match self {
            Self::Missing => Errno::ENOENT,
            Self::Denied => Errno::EACCES,
            Self::Precondition | Self::Io => Errno::EIO,
        })
    }
}

struct AwsS3Ops {
    client: aws_sdk_s3::Client,
}

impl S3Ops for AwsS3Ops {
    fn head(&self, request: HeadRequest) -> Result<HeadResult, RemoteError> {
        let client = self.client.clone();
        run_sdk(async move {
            let output = client
                .head_object()
                .bucket(request.bucket)
                .key(request.key)
                .send()
                .await
                .map_err(|err| classify_sdk_error(&err))?;
            let len = output
                .content_length()
                .and_then(|len| u64::try_from(len).ok())
                .ok_or(RemoteError::Io)?;
            let etag = output
                .e_tag()
                .filter(|etag| !etag.is_empty())
                .ok_or(RemoteError::Io)?;
            Ok(HeadResult {
                len,
                etag: etag.to_owned(),
            })
        })
    }

    fn list(&self, request: ListRequest) -> Result<ListResult, RemoteError> {
        let client = self.client.clone();
        run_sdk(async move {
            let delimiter = Some(request.delimiter).filter(|value| !value.is_empty());
            let output = client
                .list_objects_v2()
                .bucket(request.bucket)
                .prefix(request.prefix)
                .set_delimiter(delimiter)
                .set_max_keys(request.max_keys)
                .set_continuation_token(request.continuation)
                .send()
                .await
                .map_err(|err| classify_sdk_error(&err))?;

            let objects = output
                .contents()
                .iter()
                .map(|object| {
                    Ok(ListedObject {
                        key: object.key().ok_or(RemoteError::Io)?.to_owned(),
                        size: u64::try_from(object.size().ok_or(RemoteError::Io)?)
                            .map_err(|_| RemoteError::Io)?,
                    })
                })
                .collect::<Result<Vec<_>, RemoteError>>()?;
            let common_prefixes = output
                .common_prefixes()
                .iter()
                .map(|entry| entry.prefix().map(str::to_owned).ok_or(RemoteError::Io))
                .collect::<Result<Vec<_>, RemoteError>>()?;
            let truncated = require_list_truncated(output.is_truncated())?;
            Ok(ListResult {
                objects,
                common_prefixes,
                truncated,
                next_continuation: output.next_continuation_token().map(str::to_owned),
            })
        })
    }

    fn get(&self, request: GetRequest) -> Result<GetResult, RemoteError> {
        let client = self.client.clone();
        run_sdk(async move {
            let output = client
                .get_object()
                .bucket(request.bucket)
                .key(request.key)
                .range(request.range)
                .if_match(request.if_match.clone())
                .send()
                .await
                .map_err(|err| classify_sdk_error(&err))?;
            let content_length = output
                .content_length()
                .and_then(|len| u64::try_from(len).ok())
                .ok_or(RemoteError::Io)?;
            let etag = output.e_tag().map(str::to_owned);
            let content_range = output.content_range().map(str::to_owned);
            validate_get_headers(
                request.expected_len,
                &request.if_match,
                &request.expected_content_range,
                content_length,
                etag.as_deref(),
                content_range.as_deref(),
            )?;
            let expected_len =
                usize::try_from(request.expected_len).map_err(|_| RemoteError::Io)?;
            let mut reader = output.body.into_async_read();
            let body = read_exact_bounded(&mut reader, expected_len).await?;
            Ok(GetResult {
                body,
                content_length,
                etag,
                content_range,
            })
        })
    }

    fn put(&self, request: PutRequest) -> Result<(), RemoteError> {
        let client = self.client.clone();
        run_sdk(async move {
            let mut call = client
                .put_object()
                .bucket(request.bucket)
                .key(request.key)
                .content_length(i64::try_from(request.body.len()).map_err(|_| RemoteError::Io)?)
                .body(aws_sdk_s3::primitives::ByteStream::from(request.body));
            call = match request.precondition {
                Precondition::None => call,
                Precondition::Absent => call.if_none_match("*"),
                Precondition::Match(etag) => call.if_match(etag),
            };
            call.send()
                .await
                .map_err(|err| classify_sdk_error(&err))
                .map(|_| ())
        })
    }

    fn delete(&self, request: DeleteRequest) -> Result<(), RemoteError> {
        let client = self.client.clone();
        run_sdk(async move {
            let mut call = client
                .delete_object()
                .bucket(request.bucket)
                .key(request.key);
            // DeleteObject has no If-None-Match; an absent-key precondition is
            // already satisfied by the delete itself.
            if let Precondition::Match(etag) = request.precondition {
                call = call.if_match(etag);
            }
            call.send()
                .await
                .map_err(|err| classify_sdk_error(&err))
                .map(|_| ())
        })
    }

    fn copy(&self, request: CopyRequest) -> Result<(), RemoteError> {
        let client = self.client.clone();
        run_sdk(async move {
            let source = format!("{}/{}", request.bucket, uri_encode_key(&request.source_key));
            let mut call = client
                .copy_object()
                .bucket(request.bucket)
                .key(request.key)
                .copy_source(source);
            if let Precondition::Match(etag) = request.source_if_match {
                call = call.copy_source_if_match(etag);
            }
            call.send()
                .await
                .map_err(|err| classify_sdk_error(&err))
                .map(|_| ())
        })
    }

    fn create_upload(&self, request: CreateUploadRequest) -> Result<String, RemoteError> {
        let client = self.client.clone();
        run_sdk(async move {
            let output = client
                .create_multipart_upload()
                .bucket(request.bucket)
                .key(request.key)
                .send()
                .await
                .map_err(|err| classify_sdk_error(&err))?;
            output
                .upload_id()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .ok_or(RemoteError::Io)
        })
    }

    fn upload_part(&self, request: UploadPartRequest) -> Result<String, RemoteError> {
        let client = self.client.clone();
        run_sdk(async move {
            let output = client
                .upload_part()
                .bucket(request.bucket)
                .key(request.key)
                .upload_id(request.upload_id)
                .part_number(request.part_number)
                .content_length(i64::try_from(request.body.len()).map_err(|_| RemoteError::Io)?)
                .body(aws_sdk_s3::primitives::ByteStream::from(request.body))
                .send()
                .await
                .map_err(|err| classify_sdk_error(&err))?;
            output
                .e_tag()
                .filter(|etag| !etag.is_empty())
                .map(str::to_owned)
                .ok_or(RemoteError::Io)
        })
    }

    fn complete_upload(&self, request: CompleteUploadRequest) -> Result<(), RemoteError> {
        let client = self.client.clone();
        run_sdk(async move {
            let parts = request
                .parts
                .into_iter()
                .map(|part| {
                    aws_sdk_s3::types::CompletedPart::builder()
                        .part_number(part.part_number)
                        .e_tag(part.etag)
                        .build()
                })
                .collect();
            let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .set_parts(Some(parts))
                .build();
            let mut call = client
                .complete_multipart_upload()
                .bucket(request.bucket)
                .key(request.key)
                .upload_id(request.upload_id)
                .multipart_upload(completed);
            call = match request.precondition {
                Precondition::None => call,
                Precondition::Absent => call.if_none_match("*"),
                Precondition::Match(etag) => call.if_match(etag),
            };
            call.send()
                .await
                .map_err(|err| classify_sdk_error(&err))
                .map(|_| ())
        })
    }

    fn abort_upload(&self, request: AbortUploadRequest) -> Result<(), RemoteError> {
        let client = self.client.clone();
        run_sdk(async move {
            client
                .abort_multipart_upload()
                .bucket(request.bucket)
                .key(request.key)
                .upload_id(request.upload_id)
                .send()
                .await
                .map_err(|err| classify_sdk_error(&err))
                .map(|_| ())
        })
    }
}

/// Percent-encodes a key for the `x-amz-copy-source` header, which carries the
/// source as a URI path rather than a plain header value.
fn uri_encode_key(key: &str) -> String {
    let mut encoded = String::with_capacity(key.len());
    for byte in key.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                encoded.push(char::from(byte));
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn validate_get_headers(
    expected_len: u64,
    expected_etag: &str,
    expected_content_range: &str,
    content_length: u64,
    etag: Option<&str>,
    content_range: Option<&str>,
) -> Result<(), RemoteError> {
    if content_length != expected_len
        || etag != Some(expected_etag)
        || content_range != Some(expected_content_range)
    {
        return Err(RemoteError::Io);
    }
    Ok(())
}

async fn read_exact_bounded<R>(reader: &mut R, expected_len: usize) -> Result<Vec<u8>, RemoteError>
where
    R: AsyncRead + Unpin,
{
    let limit = expected_len.checked_add(1).ok_or(RemoteError::Io)?;
    let mut body = Vec::with_capacity(limit);
    reader
        .take(u64::try_from(limit).map_err(|_| RemoteError::Io)?)
        .read_to_end(&mut body)
        .await
        .map_err(|_| RemoteError::Io)?;
    if body.len() != expected_len {
        return Err(RemoteError::Io);
    }
    Ok(body)
}

fn require_list_truncated(value: Option<bool>) -> Result<bool, RemoteError> {
    value.ok_or(RemoteError::Io)
}

fn classify_sdk_error<E>(error: &SdkError<E>) -> RemoteError
where
    E: ProvideErrorMetadata,
{
    let code = error
        .as_service_error()
        .and_then(ProvideErrorMetadata::code);
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    classify_remote(code, status)
}

fn classify_remote(code: Option<&str>, status: Option<u16>) -> RemoteError {
    match (code, status) {
        (Some("NoSuchKey" | "NoSuchBucket" | "NotFound" | "404"), _) | (_, Some(404)) => {
            RemoteError::Missing
        }
        (
            Some("AccessDenied" | "Unauthorized" | "InvalidAccessKeyId" | "SignatureDoesNotMatch"),
            _,
        )
        | (_, Some(401 | 403)) => RemoteError::Denied,
        (Some("PreconditionFailed" | "412" | "ConditionalRequestConflict" | "409"), _)
        | (_, Some(409 | 412)) => RemoteError::Precondition,
        _ => RemoteError::Io,
    }
}

fn run_sdk<F, T>(future: F) -> Result<T, RemoteError>
where
    F: Future<Output = Result<T, RemoteError>> + Send + 'static,
    T: Send + 'static,
{
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, ()>> = OnceLock::new();
    let runtime = RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("tinysandbox-s3")
                .build()
                .map_err(|_| ())
        })
        .as_ref()
        .map_err(|_| RemoteError::Io)?;

    let (sender, receiver) = mpsc::sync_channel(1);
    runtime.spawn(async move {
        let _ = sender.send(future.await);
    });
    receiver.recv().map_err(|_| RemoteError::Io)?
}
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread;

    use super::*;

    #[derive(Debug)]
    enum Call {
        Head(HeadRequest, Result<HeadResult, RemoteError>),
        List(ListRequest, Result<ListResult, RemoteError>),
        Get(GetRequest, Result<GetResult, RemoteError>),
        Put(PutRequest, Result<(), RemoteError>),
        Delete(DeleteRequest, Result<(), RemoteError>),
        Copy(CopyRequest, Result<(), RemoteError>),
    }

    #[derive(Debug)]
    struct FakeOps {
        calls: Mutex<VecDeque<Call>>,
    }

    impl FakeOps {
        fn new(calls: Vec<Call>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(calls.into()),
            })
        }

        fn next(&self) -> Call {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .expect("unexpected S3 operation")
        }

        fn assert_done(&self) {
            assert!(
                self.calls
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_empty(),
                "not all scripted S3 operations were consumed"
            );
        }
    }

    impl S3Ops for FakeOps {
        fn head(&self, request: HeadRequest) -> Result<HeadResult, RemoteError> {
            match self.next() {
                Call::Head(expected, result) => {
                    assert_eq!(request, expected);
                    result
                }
                other => panic!("expected head call, got {other:?}"),
            }
        }

        fn list(&self, request: ListRequest) -> Result<ListResult, RemoteError> {
            match self.next() {
                Call::List(expected, result) => {
                    assert_eq!(request, expected);
                    result
                }
                other => panic!("expected list call, got {other:?}"),
            }
        }

        fn get(&self, request: GetRequest) -> Result<GetResult, RemoteError> {
            match self.next() {
                Call::Get(expected, result) => {
                    assert_eq!(request, expected);
                    result
                }
                other => panic!("expected get call, got {other:?}"),
            }
        }

        fn put(&self, request: PutRequest) -> Result<(), RemoteError> {
            match self.next() {
                Call::Put(expected, result) => {
                    assert_eq!(request, expected);
                    result
                }
                other => panic!("expected put call, got {other:?}"),
            }
        }

        fn delete(&self, request: DeleteRequest) -> Result<(), RemoteError> {
            match self.next() {
                Call::Delete(expected, result) => {
                    assert_eq!(request, expected);
                    result
                }
                other => panic!("expected delete call, got {other:?}"),
            }
        }

        fn copy(&self, request: CopyRequest) -> Result<(), RemoteError> {
            match self.next() {
                Call::Copy(expected, result) => {
                    assert_eq!(request, expected);
                    result
                }
                other => panic!("expected copy call, got {other:?}"),
            }
        }

        fn create_upload(&self, _request: CreateUploadRequest) -> Result<String, RemoteError> {
            panic!("scripted fake does not expect a multipart upload")
        }

        fn upload_part(&self, _request: UploadPartRequest) -> Result<String, RemoteError> {
            panic!("scripted fake does not expect a multipart upload")
        }

        fn complete_upload(&self, _request: CompleteUploadRequest) -> Result<(), RemoteError> {
            panic!("scripted fake does not expect a multipart upload")
        }

        fn abort_upload(&self, _request: AbortUploadRequest) -> Result<(), RemoteError> {
            panic!("scripted fake does not expect a multipart upload")
        }
    }

    /// An in-memory bucket with S3-shaped listing, precondition, and multipart
    /// behavior, used for round-trip tests where exact requests do not matter.
    #[derive(Debug, Default)]
    struct BucketState {
        objects: BTreeMap<String, StoredObject>,
        uploads: BTreeMap<String, PendingUpload>,
        next_etag: u64,
        next_upload: u64,
        aborted: Vec<String>,
    }

    #[derive(Debug, Clone)]
    struct StoredObject {
        body: Vec<u8>,
        etag: String,
    }

    #[derive(Debug, Clone)]
    struct PendingUpload {
        key: String,
        parts: BTreeMap<i32, Vec<u8>>,
    }

    #[derive(Debug, Default)]
    struct FakeBucket {
        state: Mutex<BucketState>,
    }

    impl FakeBucket {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn state(&self) -> std::sync::MutexGuard<'_, BucketState> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn seed(self: &Arc<Self>, key: &str, body: &[u8]) {
            let mut state = self.state();
            let etag = state.mint_etag();
            state.objects.insert(
                key.to_owned(),
                StoredObject {
                    body: body.to_vec(),
                    etag,
                },
            );
        }

        fn body(&self, key: &str) -> Option<Vec<u8>> {
            self.state()
                .objects
                .get(key)
                .map(|object| object.body.clone())
        }

        fn keys(&self) -> Vec<String> {
            self.state().objects.keys().cloned().collect()
        }

        fn open_uploads(&self) -> usize {
            self.state().uploads.len()
        }

        fn aborted(&self) -> Vec<String> {
            self.state().aborted.clone()
        }
    }

    impl BucketState {
        fn mint_etag(&mut self) -> String {
            self.next_etag += 1;
            format!("\"etag-{}\"", self.next_etag)
        }

        fn check(&self, key: &str, precondition: &Precondition) -> Result<(), RemoteError> {
            match precondition {
                Precondition::None => Ok(()),
                Precondition::Absent => {
                    if self.objects.contains_key(key) {
                        return Err(RemoteError::Precondition);
                    }
                    Ok(())
                }
                Precondition::Match(etag) => match self.objects.get(key) {
                    Some(object) if &object.etag == etag => Ok(()),
                    Some(_) => Err(RemoteError::Precondition),
                    None => Err(RemoteError::Missing),
                },
            }
        }

        fn store(&mut self, key: String, body: Vec<u8>) {
            let etag = self.mint_etag();
            self.objects.insert(key, StoredObject { body, etag });
        }
    }

    impl S3Ops for FakeBucket {
        fn head(&self, request: HeadRequest) -> Result<HeadResult, RemoteError> {
            let state = self.state();
            let object = state
                .objects
                .get(&request.key)
                .ok_or(RemoteError::Missing)?;
            Ok(HeadResult {
                len: object.body.len() as u64,
                etag: object.etag.clone(),
            })
        }

        fn list(&self, request: ListRequest) -> Result<ListResult, RemoteError> {
            let state = self.state();
            let mut objects = Vec::new();
            let mut common_prefixes = BTreeSet::new();
            for (key, object) in &state.objects {
                let Some(relative) = key.strip_prefix(&request.prefix) else {
                    continue;
                };
                if let Some(after) = &request.continuation
                    && key <= after
                {
                    continue;
                }
                match (!request.delimiter.is_empty())
                    .then(|| relative.find(&request.delimiter))
                    .flatten()
                {
                    Some(index) => {
                        common_prefixes.insert(format!(
                            "{}{}{}",
                            request.prefix,
                            &relative[..index],
                            request.delimiter
                        ));
                    }
                    None => objects.push(ListedObject {
                        key: key.clone(),
                        size: object.body.len() as u64,
                    }),
                }
            }

            let common_prefixes: Vec<String> = common_prefixes.into_iter().collect();
            let limit = request
                .max_keys
                .and_then(|max| usize::try_from(max).ok())
                .unwrap_or(usize::MAX);
            let truncated = objects.len() + common_prefixes.len() > limit;
            if truncated {
                objects.truncate(limit);
            }
            let next_continuation = truncated
                .then(|| objects.last().map(|object| object.key.clone()))
                .flatten();
            Ok(ListResult {
                objects,
                common_prefixes: if truncated {
                    Vec::new()
                } else {
                    common_prefixes
                },
                truncated,
                next_continuation,
            })
        }

        fn get(&self, request: GetRequest) -> Result<GetResult, RemoteError> {
            let state = self.state();
            let object = state
                .objects
                .get(&request.key)
                .ok_or(RemoteError::Missing)?;
            if object.etag != request.if_match {
                return Err(RemoteError::Precondition);
            }
            let range = request
                .range
                .strip_prefix("bytes=")
                .and_then(|range| range.split_once('-'))
                .ok_or(RemoteError::Io)?;
            let start: usize = range.0.parse().map_err(|_| RemoteError::Io)?;
            let end: usize = range.1.parse().map_err(|_| RemoteError::Io)?;
            let body = object
                .body
                .get(start..=end)
                .ok_or(RemoteError::Io)?
                .to_vec();
            Ok(GetResult {
                content_length: body.len() as u64,
                body,
                etag: Some(object.etag.clone()),
                content_range: Some(format!("bytes {start}-{end}/{}", object.body.len())),
            })
        }

        fn put(&self, request: PutRequest) -> Result<(), RemoteError> {
            let mut state = self.state();
            state.check(&request.key, &request.precondition)?;
            state.store(request.key, request.body);
            Ok(())
        }

        fn delete(&self, request: DeleteRequest) -> Result<(), RemoteError> {
            let mut state = self.state();
            state.check(&request.key, &request.precondition)?;
            state
                .objects
                .remove(&request.key)
                .ok_or(RemoteError::Missing)
                .map(|_| ())
        }

        fn copy(&self, request: CopyRequest) -> Result<(), RemoteError> {
            let mut state = self.state();
            state.check(&request.source_key, &request.source_if_match)?;
            let source = state
                .objects
                .get(&request.source_key)
                .ok_or(RemoteError::Missing)?
                .body
                .clone();
            state.store(request.key, source);
            Ok(())
        }

        fn create_upload(&self, request: CreateUploadRequest) -> Result<String, RemoteError> {
            let mut state = self.state();
            state.next_upload += 1;
            let id = format!("upload-{}", state.next_upload);
            state.uploads.insert(
                id.clone(),
                PendingUpload {
                    key: request.key,
                    parts: BTreeMap::new(),
                },
            );
            Ok(id)
        }

        fn upload_part(&self, request: UploadPartRequest) -> Result<String, RemoteError> {
            let mut state = self.state();
            let upload = state
                .uploads
                .get_mut(&request.upload_id)
                .ok_or(RemoteError::Missing)?;
            assert_eq!(upload.key, request.key, "part uploaded to the wrong key");
            upload.parts.insert(request.part_number, request.body);
            Ok(format!("\"part-{}\"", request.part_number))
        }

        fn complete_upload(&self, request: CompleteUploadRequest) -> Result<(), RemoteError> {
            let mut state = self.state();
            let upload = state
                .uploads
                .get(&request.upload_id)
                .ok_or(RemoteError::Missing)?
                .clone();
            state.check(&request.key, &request.precondition)?;
            assert_eq!(
                request
                    .parts
                    .iter()
                    .map(|part| part.part_number)
                    .collect::<Vec<_>>(),
                (1..=request.parts.len() as i32).collect::<Vec<_>>(),
                "parts must be completed in contiguous ascending order"
            );
            let mut body = Vec::new();
            for part in &request.parts {
                body.extend_from_slice(upload.parts.get(&part.part_number).ok_or(RemoteError::Io)?);
            }
            state.uploads.remove(&request.upload_id);
            state.store(request.key, body);
            Ok(())
        }

        fn abort_upload(&self, request: AbortUploadRequest) -> Result<(), RemoteError> {
            let mut state = self.state();
            state.aborted.push(request.upload_id.clone());
            state
                .uploads
                .remove(&request.upload_id)
                .ok_or(RemoteError::Missing)
                .map(|_| ())
        }
    }

    struct BridgedOps;

    impl S3Ops for BridgedOps {
        fn head(&self, request: HeadRequest) -> Result<HeadResult, RemoteError> {
            run_sdk(async move {
                if request.key == "file" {
                    Ok(HeadResult {
                        len: 8,
                        etag: "etag".into(),
                    })
                } else {
                    Err(RemoteError::Missing)
                }
            })
        }

        fn list(&self, _request: ListRequest) -> Result<ListResult, RemoteError> {
            run_sdk(async { Ok(empty_list()) })
        }

        fn get(&self, request: GetRequest) -> Result<GetResult, RemoteError> {
            run_sdk(async move {
                if request.key != "file" || request.if_match != "etag" {
                    return Err(RemoteError::Precondition);
                }
                let range = request
                    .range
                    .strip_prefix("bytes=")
                    .and_then(|range| range.split_once('-'))
                    .ok_or(RemoteError::Io)?;
                let start = range.0.parse::<usize>().map_err(|_| RemoteError::Io)?;
                let end = range.1.parse::<usize>().map_err(|_| RemoteError::Io)?;
                let body = b"contents"
                    .get(start..=end)
                    .ok_or(RemoteError::Io)?
                    .to_vec();
                Ok(GetResult {
                    content_length: body.len() as u64,
                    body,
                    etag: Some("etag".into()),
                    content_range: Some(format!("bytes {start}-{end}/8")),
                })
            })
        }

        fn put(&self, request: PutRequest) -> Result<(), RemoteError> {
            run_sdk(async move {
                assert_eq!(request.key, "written");
                Ok(())
            })
        }

        fn delete(&self, _request: DeleteRequest) -> Result<(), RemoteError> {
            run_sdk(async { Ok(()) })
        }

        fn copy(&self, _request: CopyRequest) -> Result<(), RemoteError> {
            run_sdk(async { Ok(()) })
        }

        fn create_upload(&self, _request: CreateUploadRequest) -> Result<String, RemoteError> {
            run_sdk(async { Ok("upload".to_owned()) })
        }

        fn upload_part(&self, _request: UploadPartRequest) -> Result<String, RemoteError> {
            run_sdk(async { Ok("\"part\"".to_owned()) })
        }

        fn complete_upload(&self, _request: CompleteUploadRequest) -> Result<(), RemoteError> {
            run_sdk(async { Ok(()) })
        }

        fn abort_upload(&self, _request: AbortUploadRequest) -> Result<(), RemoteError> {
            run_sdk(async { Ok(()) })
        }
    }

    fn list_request(prefix: &str, continuation: Option<&str>) -> ListRequest {
        ListRequest {
            bucket: "bucket".to_owned(),
            prefix: prefix.to_owned(),
            delimiter: "/".to_owned(),
            continuation: continuation.map(str::to_owned),
            max_keys: None,
        }
    }

    fn probe_request(prefix: &str) -> ListRequest {
        ListRequest {
            bucket: "bucket".to_owned(),
            prefix: prefix.to_owned(),
            delimiter: "/".to_owned(),
            continuation: None,
            max_keys: Some(1),
        }
    }

    fn empty_list() -> ListResult {
        ListResult {
            objects: vec![],
            common_prefixes: vec![],
            truncated: false,
            next_continuation: None,
        }
    }

    fn file_head(key: &str, len: u64, etag: &str) -> Call {
        Call::Head(
            HeadRequest {
                bucket: "bucket".to_owned(),
                key: key.to_owned(),
            },
            Ok(HeadResult {
                len,
                etag: etag.to_owned(),
            }),
        )
    }

    fn vfs(fake: Arc<FakeOps>, prefix: Option<&str>) -> S3Vfs {
        S3Vfs::with_ops(fake, "bucket".to_owned(), prefix, S3VfsConfig::default())
            .expect("valid fake VFS")
    }

    fn bucket_vfs(bucket: &Arc<FakeBucket>, prefix: Option<&str>) -> S3Vfs {
        configured_bucket_vfs(bucket, prefix, S3VfsConfig::default())
    }

    fn configured_bucket_vfs(
        bucket: &Arc<FakeBucket>,
        prefix: Option<&str>,
        config: S3VfsConfig,
    ) -> S3Vfs {
        S3Vfs::with_ops(
            Arc::clone(bucket) as Arc<dyn S3Ops>,
            "bucket".to_owned(),
            prefix,
            config,
        )
        .expect("valid bucket VFS")
    }

    fn assert_errno<T>(result: VfsResult<T>, errno: Errno) {
        match result {
            Ok(_) => panic!("operation should fail"),
            Err(error) => assert_eq!(error.errno(), errno),
        }
    }

    fn write_file(vfs: &S3Vfs, path: &str, data: &[u8]) -> VfsResult<()> {
        let handle = vfs.open(path, OpenMode::write_only().create().truncate())?;
        let result = write_all(vfs, handle, data);
        let close = vfs.close(handle);
        result.and(close)
    }

    fn write_all(vfs: &S3Vfs, handle: FileHandle, data: &[u8]) -> VfsResult<()> {
        let mut written = 0;
        while written < data.len() {
            written += vfs.write_at(handle, written as u64, &data[written..])?;
        }
        Ok(())
    }

    fn read_file(vfs: &S3Vfs, path: &str) -> VfsResult<Vec<u8>> {
        let handle = vfs.open(path, OpenMode::read_only())?;
        let mut out = Vec::new();
        let mut buf = [0; 64];
        loop {
            let read = vfs.read_at(handle, out.len() as u64, &mut buf)?;
            if read == 0 {
                break;
            }
            out.extend_from_slice(&buf[..read]);
        }
        vfs.close(handle)?;
        Ok(out)
    }

    #[test]
    fn construction_normalizes_prefix_and_rejects_invalid_configuration() {
        let fake = FakeOps::new(vec![]);
        let rooted = vfs(Arc::clone(&fake), Some("/team/data/"));
        assert_eq!(rooted.root_prefix, "team/data/");
        assert_eq!(vfs(Arc::clone(&fake), Some("///")).root_prefix, "");
        assert_errno(
            S3Vfs::with_ops(
                Arc::clone(&fake) as Arc<dyn S3Ops>,
                "".into(),
                None,
                S3VfsConfig::default(),
            ),
            Errno::EINVAL,
        );
        for prefix in ["a//b", "a/./b", "a/../b", "a\0b"] {
            assert_errno(
                S3Vfs::with_ops(
                    Arc::clone(&fake) as Arc<dyn S3Ops>,
                    "bucket".into(),
                    Some(prefix),
                    S3VfsConfig::default(),
                ),
                Errno::EINVAL,
            );
        }
    }

    #[test]
    fn default_config_is_writable_with_a_bounded_edit_limit() {
        let config = S3VfsConfig::default();
        assert!(!config.read_only);
        assert!(config.directory_rename);
        assert!(config.conditional_writes);
        assert_eq!(config.max_edit_bytes, 32 * 1024 * 1024);
        assert!(S3VfsConfig::read_only().read_only);
    }

    #[test]
    fn empty_root_is_virtual_and_exactly_prefix_scoped() {
        let fake = FakeOps::new(vec![Call::List(
            list_request("root/", None),
            Ok(empty_list()),
        )]);
        let vfs = vfs(Arc::clone(&fake), Some("/root/"));
        assert!(vfs.stat("/").expect("root stat").is_dir());
        assert!(vfs.readdir("/").expect("root listing").is_empty());
        fake.assert_done();
    }

    #[test]
    fn paginated_listing_is_sorted_deduplicated_and_directory_wins() {
        let fake = FakeOps::new(vec![
            Call::List(
                list_request("root/", None),
                Ok(ListResult {
                    objects: vec![
                        ListedObject {
                            key: "root/z".into(),
                            size: 9,
                        },
                        ListedObject {
                            key: "root/a".into(),
                            size: 1,
                        },
                        ListedObject {
                            key: "sibling-secret".into(),
                            size: 99,
                        },
                        ListedObject {
                            key: "root/.".into(),
                            size: 0,
                        },
                        ListedObject {
                            key: "root/nul\0name".into(),
                            size: 0,
                        },
                    ],
                    common_prefixes: vec![
                        "root/z/".into(),
                        "root/../".into(),
                        "root/not/immediate/".into(),
                    ],
                    truncated: true,
                    next_continuation: Some("page-2".into()),
                }),
            ),
            Call::List(
                list_request("root/", Some("page-2")),
                Ok(ListResult {
                    objects: vec![ListedObject {
                        key: "root/a".into(),
                        size: 1,
                    }],
                    common_prefixes: vec!["root/b/".into(), "root/z/".into()],
                    truncated: false,
                    next_continuation: None,
                }),
            ),
        ]);
        let entries = vfs(Arc::clone(&fake), Some("root"))
            .readdir("/")
            .expect("list root");
        assert_eq!(
            entries,
            vec![
                DirEntry {
                    name: "a".into(),
                    metadata: Metadata {
                        file_type: FileType::File,
                        len: 1
                    },
                },
                DirEntry {
                    name: "b".into(),
                    metadata: Metadata {
                        file_type: FileType::Directory,
                        len: 0
                    },
                },
                DirEntry {
                    name: "z".into(),
                    metadata: Metadata {
                        file_type: FileType::Directory,
                        len: 0
                    },
                },
            ]
        );
        fake.assert_done();
    }

    #[test]
    fn marker_implicit_directory_and_collision_agree_across_operations() {
        let dir_page = || ListResult {
            objects: vec![
                ListedObject {
                    key: "root/a/".into(),
                    size: 0,
                },
                ListedObject {
                    key: "root/a/file".into(),
                    size: 3,
                },
            ],
            common_prefixes: vec![],
            truncated: false,
            next_continuation: None,
        };
        let fake = FakeOps::new(vec![
            Call::List(list_request("root/a/", None), Ok(dir_page())),
            Call::List(list_request("root/a/", None), Ok(dir_page())),
            Call::List(list_request("root/a/", None), Ok(dir_page())),
        ]);
        let vfs = vfs(Arc::clone(&fake), Some("root"));
        assert!(vfs.stat("/a").expect("directory stat").is_dir());
        assert_eq!(
            vfs.readdir("/a").expect("directory listing")[0].name,
            "file"
        );
        assert_errno(vfs.open("/a", OpenMode::read_only()), Errno::EISDIR);
        fake.assert_done();
    }

    #[test]
    fn marker_only_directory_exists_and_is_empty() {
        let marker_page = || ListResult {
            objects: vec![ListedObject {
                key: "root/empty/".into(),
                size: 0,
            }],
            common_prefixes: vec![],
            truncated: false,
            next_continuation: None,
        };
        let fake = FakeOps::new(vec![
            Call::List(list_request("root/empty/", None), Ok(marker_page())),
            Call::List(list_request("root/empty/", None), Ok(marker_page())),
            Call::List(list_request("root/empty/", None), Ok(marker_page())),
        ]);
        let vfs = vfs(Arc::clone(&fake), Some("root"));
        assert!(vfs.stat("/empty").expect("marker directory stat").is_dir());
        assert!(
            vfs.readdir("/empty")
                .expect("marker directory listing")
                .is_empty()
        );
        assert_errno(vfs.open("/empty", OpenMode::read_only()), Errno::EISDIR);
        fake.assert_done();
    }

    #[test]
    fn exact_object_and_descendant_collision_is_always_a_directory() {
        let child_page = || ListResult {
            objects: vec![ListedObject {
                key: "root/a/child".into(),
                size: 5,
            }],
            common_prefixes: vec![],
            truncated: false,
            next_continuation: None,
        };
        let fake = FakeOps::new(vec![
            Call::List(
                list_request("root/", None),
                Ok(ListResult {
                    objects: vec![ListedObject {
                        key: "root/a".into(),
                        size: 99,
                    }],
                    common_prefixes: vec!["root/a/".into()],
                    truncated: false,
                    next_continuation: None,
                }),
            ),
            Call::List(list_request("root/a/", None), Ok(child_page())),
            Call::List(list_request("root/a/", None), Ok(child_page())),
            Call::List(list_request("root/a/", None), Ok(child_page())),
        ]);
        let vfs = vfs(Arc::clone(&fake), Some("root"));
        let entries = vfs.readdir("/").expect("root listing");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a");
        assert!(entries[0].metadata.is_dir());
        assert!(vfs.stat("/a").expect("collision stat").is_dir());
        assert_eq!(
            vfs.readdir("/a").expect("collision readdir")[0].name,
            "child"
        );
        assert_errno(vfs.open("/a", OpenMode::read_only()), Errno::EISDIR);
        fake.assert_done();
    }

    #[test]
    fn bounded_etag_pinned_reads_and_eof_fast_paths() {
        let fake = FakeOps::new(vec![
            Call::List(list_request("root/file/", None), Ok(empty_list())),
            file_head("root/file", 10, "\"etag\""),
            Call::Get(
                GetRequest {
                    bucket: "bucket".into(),
                    key: "root/file".into(),
                    range: "bytes=2-5".into(),
                    if_match: "\"etag\"".into(),
                    expected_len: 4,
                    expected_content_range: "bytes 2-5/10".into(),
                },
                Ok(GetResult {
                    body: b"2345".to_vec(),
                    content_length: 4,
                    etag: Some("\"etag\"".into()),
                    content_range: Some("bytes 2-5/10".into()),
                }),
            ),
        ]);
        let vfs = vfs(Arc::clone(&fake), Some("root"));
        let handle = vfs.open("/file", OpenMode::read_only()).expect("open file");
        let mut empty = [];
        assert_eq!(vfs.read_at(handle, 0, &mut empty).expect("empty read"), 0);
        let mut eof = [0; 4];
        assert_eq!(vfs.read_at(handle, 10, &mut eof).expect("EOF read"), 0);
        let mut buf = [0; 4];
        assert_eq!(vfs.read_at(handle, 2, &mut buf).expect("range read"), 4);
        assert_eq!(&buf, b"2345");
        fake.assert_done();
    }

    #[test]
    fn partial_tail_read_is_capped_to_object_length() {
        let fake = FakeOps::new(vec![
            Call::List(list_request("file/", None), Ok(empty_list())),
            file_head("file", 5, "e"),
            Call::Get(
                GetRequest {
                    bucket: "bucket".into(),
                    key: "file".into(),
                    range: "bytes=3-4".into(),
                    if_match: "e".into(),
                    expected_len: 2,
                    expected_content_range: "bytes 3-4/5".into(),
                },
                Ok(GetResult {
                    body: b"lo".to_vec(),
                    content_length: 2,
                    etag: Some("e".into()),
                    content_range: Some("bytes 3-4/5".into()),
                }),
            ),
        ]);
        let vfs = vfs(Arc::clone(&fake), None);
        let handle = vfs.open("/file", OpenMode::read_only()).expect("open");
        let mut buf = [0; 8];
        assert_eq!(vfs.read_at(handle, 3, &mut buf).expect("tail read"), 2);
        assert_eq!(&buf[..2], b"lo");
        fake.assert_done();
    }

    #[test]
    fn malformed_short_or_changed_get_responses_are_eio() {
        for response in [
            Err(RemoteError::Precondition),
            Ok(GetResult {
                body: vec![1],
                content_length: 2,
                etag: Some("e".into()),
                content_range: Some("bytes 0-1/2".into()),
            }),
            Ok(GetResult {
                body: vec![1, 2],
                content_length: 2,
                etag: Some("changed".into()),
                content_range: Some("bytes 0-1/2".into()),
            }),
            Ok(GetResult {
                body: vec![1, 2],
                content_length: 2,
                etag: Some("e".into()),
                content_range: Some("malformed".into()),
            }),
        ] {
            let fake = FakeOps::new(vec![
                Call::List(list_request("file/", None), Ok(empty_list())),
                file_head("file", 2, "e"),
                Call::Get(
                    GetRequest {
                        bucket: "bucket".into(),
                        key: "file".into(),
                        range: "bytes=0-1".into(),
                        if_match: "e".into(),
                        expected_len: 2,
                        expected_content_range: "bytes 0-1/2".into(),
                    },
                    response,
                ),
            ]);
            let vfs = vfs(fake, None);
            let handle = vfs.open("/file", OpenMode::read_only()).expect("open");
            assert_errno(vfs.read_at(handle, 0, &mut [0; 2]), Errno::EIO);
        }
    }

    #[test]
    fn production_body_reader_is_bounded_and_validates_before_reading() {
        assert_eq!(
            validate_get_headers(4, "e", "bytes 0-3/8", 8, Some("e"), Some("bytes 0-3/8")),
            Err(RemoteError::Io),
            "an ignored Range response is rejected from its oversized Content-Length"
        );
        assert_eq!(
            validate_get_headers(4, "e", "bytes 0-3/8", 4, Some("other"), Some("bytes 0-3/8")),
            Err(RemoteError::Io)
        );
        assert_eq!(
            validate_get_headers(4, "e", "bytes 0-3/8", 4, Some("e"), Some("malformed")),
            Err(RemoteError::Io)
        );
        assert_eq!(require_list_truncated(None), Err(RemoteError::Io));
        assert_eq!(require_list_truncated(Some(false)), Ok(false));
        assert_eq!(require_list_truncated(Some(true)), Ok(true));

        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let mut exact = tokio::io::repeat(b'x').take(4);
            assert_eq!(
                read_exact_bounded(&mut exact, 4)
                    .await
                    .expect("exact finite body"),
                b"xxxx"
            );
            // An infinite or overlong source is rejected after consuming only
            // one sentinel byte beyond the requested range.
            let mut infinite = tokio::io::repeat(b'x');
            assert_eq!(
                read_exact_bounded(&mut infinite, 4).await,
                Err(RemoteError::Io)
            );
            let mut short = tokio::io::empty();
            assert_eq!(
                read_exact_bounded(&mut short, 1).await,
                Err(RemoteError::Io)
            );
        });
    }

    #[test]
    fn read_only_configuration_rejects_every_mutation() {
        let fake = FakeOps::new(vec![
            Call::List(list_request("file/", None), Ok(empty_list())),
            file_head("file", 1, "e"),
        ]);
        let vfs = S3Vfs::with_ops(
            Arc::clone(&fake) as Arc<dyn S3Ops>,
            "bucket".to_owned(),
            None,
            S3VfsConfig::read_only(),
        )
        .expect("valid read-only VFS");
        assert!(!vfs.is_fast());
        assert!(vfs.stats().is_none());
        assert_errno(vfs.open("/file", OpenMode::default()), Errno::EINVAL);
        for mode in [
            OpenMode::write_only(),
            OpenMode::read_write(),
            OpenMode::read_only().create(),
            OpenMode::read_only().create_new(),
            OpenMode::read_only().append(),
        ] {
            assert_errno(vfs.open("/file", mode), Errno::EACCES);
        }
        // A malformed mode is rejected before the mount policy applies.
        assert_errno(
            vfs.open("/file", OpenMode::read_only().truncate()),
            Errno::EINVAL,
        );
        for result in [
            vfs.mkdir("/x"),
            vfs.unlink("/x"),
            vfs.rmdir("/x"),
            vfs.rename("/x", "/y"),
        ] {
            assert_errno(result, Errno::EACCES);
        }
        // Path validation still precedes the write check.
        assert_errno(vfs.mkdir("relative"), Errno::EINVAL);

        let handle = vfs.open("/file", OpenMode::read_only()).expect("open");
        assert_errno(vfs.write_at(handle, 0, b"x"), Errno::EBADF);
        assert_errno(vfs.truncate(handle, 0), Errno::EINVAL);
        vfs.close(handle).expect("first close");
        assert_errno(vfs.read_at(handle, 0, &mut [0]), Errno::EBADF);
        assert_errno(vfs.write_at(handle, 0, b"x"), Errno::EBADF);
        assert_errno(vfs.truncate(handle, 0), Errno::EBADF);
        assert_errno(vfs.close(handle), Errno::EBADF);
        fake.assert_done();
    }

    #[test]
    fn parent_files_are_enotdir_and_missing_parents_are_enoent() {
        let fake = FakeOps::new(vec![
            Call::List(list_request("file/", None), Ok(empty_list())),
            file_head("file", 1, "e"),
            Call::List(list_request("missing/", None), Ok(empty_list())),
            Call::Head(
                HeadRequest {
                    bucket: "bucket".into(),
                    key: "missing".into(),
                },
                Err(RemoteError::Missing),
            ),
        ]);
        let vfs = vfs(Arc::clone(&fake), None);
        assert_errno(vfs.stat("/file/child"), Errno::ENOTDIR);
        assert_errno(vfs.stat("/missing/child"), Errno::ENOENT);
        fake.assert_done();
    }

    #[test]
    fn pagination_protocol_errors_are_eio() {
        for next in [None, Some(""), Some("same")] {
            let mut calls = vec![Call::List(
                list_request("", None),
                Ok(ListResult {
                    objects: vec![],
                    common_prefixes: vec![],
                    truncated: true,
                    next_continuation: next.map(str::to_owned),
                }),
            )];
            if next == Some("same") {
                calls.push(Call::List(
                    list_request("", Some("same")),
                    Ok(ListResult {
                        objects: vec![],
                        common_prefixes: vec![],
                        truncated: true,
                        next_continuation: Some("same".into()),
                    }),
                ));
            }
            assert_errno(vfs(FakeOps::new(calls), None).readdir("/"), Errno::EIO);
        }
    }

    #[test]
    fn all_remote_error_classes_map_to_stable_errno() {
        for (remote, errno) in [
            (RemoteError::Missing, Errno::ENOENT),
            (RemoteError::Denied, Errno::EACCES),
            (RemoteError::Precondition, Errno::EIO),
            (RemoteError::Io, Errno::EIO),
        ] {
            assert_eq!(remote.into_vfs().errno(), errno);
        }
        assert_eq!(
            exclusive_error(RemoteError::Precondition, true).errno(),
            Errno::EEXIST,
            "a failed exclusive create reports the collision"
        );
        assert_eq!(
            exclusive_error(RemoteError::Precondition, false).errno(),
            Errno::EIO,
            "a failed replace precondition reports a lost update"
        );
        for (code, status, expected) in [
            (Some("NoSuchKey"), None, RemoteError::Missing),
            (Some("NoSuchBucket"), None, RemoteError::Missing),
            (None, Some(404), RemoteError::Missing),
            (Some("AccessDenied"), None, RemoteError::Denied),
            (None, Some(401), RemoteError::Denied),
            (None, Some(403), RemoteError::Denied),
            (None, Some(412), RemoteError::Precondition),
            (
                Some("ConditionalRequestConflict"),
                None,
                RemoteError::Precondition,
            ),
            (None, Some(409), RemoteError::Precondition),
            (None, Some(500), RemoteError::Io),
            (None, None, RemoteError::Io),
        ] {
            assert_eq!(classify_remote(code, status), expected);
        }
    }

    #[test]
    fn copy_source_keys_are_uri_encoded() {
        assert_eq!(uri_encode_key("root/a b+c.txt"), "root/a%20b%2Bc.txt");
        assert_eq!(uri_encode_key("plain/key-1_2.txt~"), "plain/key-1_2.txt~");
    }

    #[test]
    fn direct_s3_vfs_calls_are_safe_inside_tokio_and_concurrently() {
        let vfs = Arc::new(
            S3Vfs::with_ops(
                Arc::new(BridgedOps),
                "bucket".into(),
                None,
                S3VfsConfig::default(),
            )
            .expect("valid bridged VFS"),
        );
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let handle = vfs
                .open("/file", OpenMode::read_only())
                .expect("open from inside Tokio");
            let mut buf = [0; 8];
            assert_eq!(
                vfs.read_at(handle, 0, &mut buf)
                    .expect("read from inside Tokio"),
                8
            );
            assert_eq!(&buf, b"contents");
            vfs.close(handle).expect("close inside Tokio");

            let written = vfs
                .open("/written", OpenMode::write_only().create().truncate())
                .expect("open write handle inside Tokio");
            vfs.write_at(written, 0, b"payload")
                .expect("write from inside Tokio");
            vfs.close(written).expect("write back inside Tokio");
        });

        let current_thread = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread test runtime");
        current_thread.block_on(async {
            let handle = vfs
                .open("/file", OpenMode::read_only())
                .expect("open from current-thread Tokio");
            let mut buf = [0; 4];
            assert_eq!(
                vfs.read_at(handle, 0, &mut buf)
                    .expect("read from current-thread Tokio"),
                4
            );
            assert_eq!(&buf, b"cont");
            vfs.close(handle).expect("close current-thread handle");
        });

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let vfs = Arc::clone(&vfs);
                thread::spawn(move || {
                    let handle = vfs.open("/file", OpenMode::read_only())?;
                    let mut buf = [0; 4];
                    let read = vfs.read_at(handle, 2, &mut buf)?;
                    vfs.close(handle)?;
                    Ok::<_, VfsError>((read, buf))
                })
            })
            .collect();
        for thread in threads {
            let (read, buf) = thread
                .join()
                .expect("thread does not panic")
                .expect("concurrent read succeeds");
            assert_eq!(read, 4);
            assert_eq!(&buf, b"nten");
        }
    }

    #[test]
    fn sdk_worker_panic_closes_channel_as_eio() {
        assert_eq!(
            run_sdk(async {
                panic!("scripted SDK worker panic");
                #[allow(unreachable_code)]
                Ok::<(), RemoteError>(())
            }),
            Err(RemoteError::Io)
        );
    }

    #[test]
    fn mutations_emit_exactly_scoped_and_guarded_requests() {
        let directory_page = || ListResult {
            objects: vec![ListedObject {
                key: "root/dir/file.txt".into(),
                size: 4,
            }],
            common_prefixes: vec![],
            truncated: false,
            next_continuation: None,
        };
        let fake = FakeOps::new(vec![
            // mkdir writes a zero-byte marker only when nothing is there.
            Call::List(list_request("root/dir/", None), Ok(empty_list())),
            Call::Head(
                HeadRequest {
                    bucket: "bucket".into(),
                    key: "root/dir".into(),
                },
                Err(RemoteError::Missing),
            ),
            Call::Put(
                PutRequest {
                    bucket: "bucket".into(),
                    key: "root/dir/".into(),
                    body: Vec::new(),
                    precondition: Precondition::Absent,
                },
                Ok(()),
            ),
            // unlink deletes the pinned revision, then keeps the directory.
            Call::List(list_request("root/dir/", None), Ok(directory_page())),
            Call::List(list_request("root/dir/file.txt/", None), Ok(empty_list())),
            file_head("root/dir/file.txt", 4, "\"live\""),
            Call::Delete(
                DeleteRequest {
                    bucket: "bucket".into(),
                    key: "root/dir/file.txt".into(),
                    precondition: Precondition::Match("\"live\"".into()),
                },
                Ok(()),
            ),
            Call::List(probe_request("root/dir/"), Ok(empty_list())),
            Call::Put(
                PutRequest {
                    bucket: "bucket".into(),
                    key: "root/dir/".into(),
                    body: Vec::new(),
                    precondition: Precondition::None,
                },
                Ok(()),
            ),
            // rename copies the pinned revision and deletes the source.
            Call::List(list_request("root/a.txt/", None), Ok(empty_list())),
            file_head("root/a.txt", 7, "\"src\""),
            Call::List(list_request("root/b.txt/", None), Ok(empty_list())),
            Call::Head(
                HeadRequest {
                    bucket: "bucket".into(),
                    key: "root/b.txt".into(),
                },
                Err(RemoteError::Missing),
            ),
            Call::Copy(
                CopyRequest {
                    bucket: "bucket".into(),
                    source_key: "root/a.txt".into(),
                    key: "root/b.txt".into(),
                    source_if_match: Precondition::Match("\"src\"".into()),
                },
                Ok(()),
            ),
            Call::Delete(
                DeleteRequest {
                    bucket: "bucket".into(),
                    key: "root/a.txt".into(),
                    precondition: Precondition::Match("\"src\"".into()),
                },
                Ok(()),
            ),
        ]);
        let vfs = vfs(Arc::clone(&fake), Some("root"));
        vfs.mkdir("/dir").expect("mkdir");
        vfs.unlink("/dir/file.txt").expect("unlink");
        vfs.rename("/a.txt", "/b.txt").expect("rename");
        fake.assert_done();
    }

    #[test]
    fn a_creating_read_handle_lands_an_empty_object() {
        let bucket = FakeBucket::new();
        let vfs = bucket_vfs(&bucket, None);

        let handle = vfs
            .open("/created.txt", OpenMode::read_only().create())
            .expect("read-only create");
        assert_errno(vfs.write_at(handle, 0, b"x"), Errno::EBADF);
        assert_eq!(vfs.read_at(handle, 0, &mut [0; 4]).expect("read"), 0);
        vfs.close(handle).expect("close creates the object");
        assert_eq!(bucket.body("created.txt").as_deref(), Some(&b""[..]));

        assert_errno(
            vfs.open("/created.txt", OpenMode::read_only().create_new()),
            Errno::EEXIST,
        );
    }

    #[test]
    fn new_files_round_trip_through_a_single_put() {
        let bucket = FakeBucket::new();
        let vfs = bucket_vfs(&bucket, Some("root"));

        write_file(&vfs, "/file.txt", b"hello").expect("create file");
        assert_eq!(bucket.body("root/file.txt").as_deref(), Some(&b"hello"[..]));
        assert_eq!(read_file(&vfs, "/file.txt").expect("read back"), b"hello");
        assert_eq!(vfs.stat("/file.txt").expect("stat").len, 5);
        assert_eq!(bucket.open_uploads(), 0, "a small object needs no upload");

        // Truncating replaces the whole object.
        write_file(&vfs, "/file.txt", b"bye").expect("replace file");
        assert_eq!(bucket.body("root/file.txt").as_deref(), Some(&b"bye"[..]));
    }

    #[test]
    fn writes_land_only_when_the_handle_closes() {
        let bucket = FakeBucket::new();
        let vfs = bucket_vfs(&bucket, None);

        let handle = vfs
            .open("/pending.txt", OpenMode::write_only().create().truncate())
            .expect("open new file");
        vfs.write_at(handle, 0, b"staged").expect("staged write");
        assert!(
            bucket.body("pending.txt").is_none(),
            "staged writes are invisible before close"
        );
        vfs.close(handle).expect("close lands the object");
        assert_eq!(bucket.body("pending.txt").as_deref(), Some(&b"staged"[..]));
    }

    #[test]
    fn touch_of_an_existing_object_preserves_its_contents() {
        let bucket = FakeBucket::new();
        bucket.seed("keep.txt", b"original");
        let vfs = bucket_vfs(&bucket, None);

        let handle = vfs
            .open("/keep.txt", OpenMode::write_only().create())
            .expect("touch existing file");
        vfs.close(handle).expect("close without writing");
        assert_eq!(
            bucket.body("keep.txt").as_deref(),
            Some(&b"original"[..]),
            "an unwritten handle must not replace the object"
        );

        let created = vfs
            .open("/fresh.txt", OpenMode::write_only().create())
            .expect("touch missing file");
        vfs.close(created).expect("close creates the object");
        assert_eq!(bucket.body("fresh.txt").as_deref(), Some(&b""[..]));
    }

    #[test]
    fn edits_read_modify_and_write_the_whole_object() {
        let bucket = FakeBucket::new();
        bucket.seed("edit.txt", b"abcdefgh");
        let vfs = bucket_vfs(&bucket, None);

        let handle = vfs
            .open("/edit.txt", OpenMode::read_write())
            .expect("open for editing");
        let mut buf = [0; 3];
        assert_eq!(vfs.read_at(handle, 2, &mut buf).expect("read"), 3);
        assert_eq!(&buf, b"cde");
        vfs.write_at(handle, 3, b"XY").expect("overwrite middle");
        vfs.close(handle).expect("write back");

        assert_eq!(bucket.body("edit.txt").as_deref(), Some(&b"abcXYfgh"[..]));
    }

    #[test]
    fn append_extends_an_existing_object() {
        let bucket = FakeBucket::new();
        bucket.seed("log.txt", b"first");
        let vfs = bucket_vfs(&bucket, None);

        let handle = vfs
            .open("/log.txt", OpenMode::write_only().create().append())
            .expect("open for append");
        vfs.write_at(handle, 0, b"-second")
            .expect("append ignores the offset");
        vfs.close(handle).expect("write back");
        assert_eq!(
            bucket.body("log.txt").as_deref(),
            Some(&b"first-second"[..])
        );
    }

    #[test]
    fn sparse_writes_and_truncation_zero_fill() {
        let bucket = FakeBucket::new();
        let vfs = bucket_vfs(&bucket, None);

        let handle = vfs
            .open("/sparse.bin", OpenMode::read_write().create().truncate())
            .expect("open new file");
        vfs.write_at(handle, 4, b"end").expect("write past the end");
        vfs.close(handle).expect("close");
        assert_eq!(
            bucket.body("sparse.bin").as_deref(),
            Some(&b"\0\0\0\0end"[..])
        );

        let grow = vfs
            .open("/sparse.bin", OpenMode::read_write())
            .expect("reopen");
        vfs.truncate(grow, 9).expect("grow");
        vfs.close(grow).expect("close");
        assert_eq!(bucket.body("sparse.bin").map(|body| body.len()), Some(9));

        let shrink = vfs
            .open("/sparse.bin", OpenMode::read_write())
            .expect("reopen");
        vfs.truncate(shrink, 2).expect("shrink");
        vfs.close(shrink).expect("close");
        assert_eq!(bucket.body("sparse.bin").as_deref(), Some(&b"\0\0"[..]));
    }

    #[test]
    fn oversized_edits_report_efbig_while_replacement_still_works() {
        let bucket = FakeBucket::new();
        bucket.seed("big.bin", &[b'x'; 64]);
        let vfs = configured_bucket_vfs(
            &bucket,
            None,
            S3VfsConfig {
                max_edit_bytes: 16,
                ..S3VfsConfig::default()
            },
        );

        let edit = vfs
            .open("/big.bin", OpenMode::read_write())
            .expect("open succeeds without downloading");
        assert_errno(vfs.read_at(edit, 0, &mut [0; 8]), Errno::EFBIG);
        assert_errno(vfs.write_at(edit, 0, b"x"), Errno::EFBIG);
        vfs.close(edit).expect("close a handle that never staged");
        assert_eq!(
            bucket.body("big.bin").map(|body| body.len()),
            Some(64),
            "a failed edit leaves the object untouched"
        );

        // Growing a staged body past the limit is the same failure.
        let grow = vfs
            .open("/small.bin", OpenMode::read_write().create().truncate())
            .expect("create small file");
        assert_errno(vfs.write_at(grow, 0, &[b'y'; 17]), Errno::EFBIG);
        assert_errno(vfs.truncate(grow, u64::MAX), Errno::EFBIG);

        // Replacing the oversized object needs no staging and still succeeds.
        write_file(&vfs, "/big.bin", b"replaced").expect("rewrite oversized object");
        assert_eq!(bucket.body("big.bin").as_deref(), Some(&b"replaced"[..]));
    }

    #[test]
    fn a_zero_edit_limit_removes_the_ceiling() {
        let bucket = FakeBucket::new();
        bucket.seed("big.bin", &[b'x'; 64]);
        let vfs = configured_bucket_vfs(
            &bucket,
            None,
            S3VfsConfig {
                max_edit_bytes: 0,
                ..S3VfsConfig::default()
            },
        );

        let handle = vfs.open("/big.bin", OpenMode::read_write()).expect("open");
        vfs.write_at(handle, 64, b"!").expect("append past the end");
        vfs.close(handle).expect("write back");
        assert_eq!(bucket.body("big.bin").map(|body| body.len()), Some(65));
    }

    #[test]
    fn long_sequential_writes_stream_through_a_multipart_upload() {
        let bucket = FakeBucket::new();
        let mut vfs = bucket_vfs(&bucket, None);
        vfs.part_size = 8;
        vfs.config.max_edit_bytes = 16;

        let handle = vfs
            .open("/stream.bin", OpenMode::write_only().create().truncate())
            .expect("open stream");
        let payload: Vec<u8> = (0..64_u8).collect();
        let mut offset = 0_u64;
        for chunk in payload.chunks(5) {
            let written = vfs
                .write_at(handle, offset, chunk)
                .expect("sequential chunk");
            offset += written as u64;
        }
        vfs.close(handle).expect("complete upload");

        assert_eq!(
            bucket.body("stream.bin").as_deref(),
            Some(&payload[..]),
            "streamed parts reassemble in order"
        );
        assert_eq!(bucket.open_uploads(), 0, "the upload is completed");
        assert!(
            bucket.aborted().is_empty(),
            "a successful stream aborts nothing"
        );
    }

    #[test]
    fn rewriting_flushed_bytes_reports_efbig_and_aborts_the_upload() {
        let bucket = FakeBucket::new();
        let mut vfs = bucket_vfs(&bucket, None);
        vfs.part_size = 8;

        let handle = vfs
            .open("/stream.bin", OpenMode::write_only().create().truncate())
            .expect("open stream");
        vfs.write_at(handle, 0, &[b'a'; 24]).expect("flush parts");
        assert_errno(vfs.write_at(handle, 0, b"z"), Errno::EFBIG);
        assert_errno(vfs.truncate(handle, 4), Errno::EFBIG);

        drop(vfs);
        assert_eq!(
            bucket.aborted().len(),
            1,
            "dropping the VFS aborts the pending upload"
        );
        assert_eq!(bucket.open_uploads(), 0);
        assert!(bucket.body("stream.bin").is_none());
    }

    #[test]
    fn exclusive_creation_and_lost_updates_are_detected() {
        let bucket = FakeBucket::new();
        let vfs = bucket_vfs(&bucket, None);

        write_file(&vfs, "/only.txt", b"first").expect("create");
        assert_errno(
            vfs.open("/only.txt", OpenMode::write_only().create_new()),
            Errno::EEXIST,
        );

        // Two exclusive handles opened before either lands: the second close
        // finds the key taken.
        let first = vfs
            .open("/race.txt", OpenMode::write_only().create_new())
            .expect("first exclusive open");
        let second = vfs
            .open("/race.txt", OpenMode::write_only().create_new())
            .expect("second exclusive open");
        vfs.write_at(first, 0, b"a").expect("write first");
        vfs.write_at(second, 0, b"b").expect("write second");
        vfs.close(first).expect("first close wins");
        assert_errno(vfs.close(second), Errno::EEXIST);
        assert_eq!(bucket.body("race.txt").as_deref(), Some(&b"a"[..]));

        // An edit whose object was replaced underneath it fails instead of
        // clobbering the newer revision.
        let edit = vfs
            .open("/only.txt", OpenMode::read_write())
            .expect("open for edit");
        vfs.write_at(edit, 0, b"S").expect("stage an edit");
        bucket.seed("only.txt", b"replaced by someone else");
        assert_errno(vfs.close(edit), Errno::EIO);
        assert_eq!(
            bucket.body("only.txt").as_deref(),
            Some(&b"replaced by someone else"[..])
        );
    }

    #[test]
    fn disabling_conditional_writes_drops_the_preconditions() {
        let bucket = FakeBucket::new();
        let vfs = configured_bucket_vfs(
            &bucket,
            None,
            S3VfsConfig {
                conditional_writes: false,
                ..S3VfsConfig::default()
            },
        );

        write_file(&vfs, "/file.txt", b"first").expect("create");
        let edit = vfs
            .open("/file.txt", OpenMode::read_write())
            .expect("open for edit");
        vfs.write_at(edit, 0, b"S").expect("stage an edit");
        bucket.seed("file.txt", b"outside");
        vfs.close(edit)
            .expect("unconditional write back overwrites");
        assert_eq!(bucket.body("file.txt").as_deref(), Some(&b"Sirst"[..]));
    }

    #[test]
    fn directories_are_created_listed_and_removed() {
        let bucket = FakeBucket::new();
        let vfs = bucket_vfs(&bucket, Some("root"));

        vfs.mkdir("/dir").expect("mkdir");
        assert_eq!(bucket.keys(), vec!["root/dir/".to_owned()]);
        assert!(vfs.stat("/dir").expect("stat").is_dir());
        assert!(vfs.readdir("/dir").expect("readdir").is_empty());
        assert_errno(vfs.mkdir("/dir"), Errno::EEXIST);
        assert_errno(vfs.open("/dir", OpenMode::read_only()), Errno::EISDIR);
        assert_errno(vfs.mkdir("/missing/child"), Errno::ENOENT);

        write_file(&vfs, "/dir/file.txt", b"data").expect("create nested file");
        assert_errno(vfs.rmdir("/dir"), Errno::ENOTEMPTY);
        assert_errno(vfs.unlink("/dir"), Errno::EISDIR);
        assert_errno(vfs.rmdir("/dir/file.txt"), Errno::ENOTDIR);
        assert_eq!(
            vfs.readdir("/dir")
                .expect("listing")
                .into_iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>(),
            vec!["file.txt"]
        );

        vfs.unlink("/dir/file.txt").expect("unlink");
        assert_errno(vfs.unlink("/dir/file.txt"), Errno::ENOENT);
        assert!(
            vfs.stat("/dir")
                .expect("emptied directory survives")
                .is_dir(),
            "removing the last child must not remove its directory"
        );
        vfs.rmdir("/dir").expect("rmdir");
        assert_errno(vfs.stat("/dir"), Errno::ENOENT);
        assert!(bucket.keys().is_empty());
    }

    #[test]
    fn an_emptied_implicit_directory_keeps_a_marker() {
        let bucket = FakeBucket::new();
        // Seeded out of band, so the directory exists only implicitly.
        bucket.seed("data/only.txt", b"payload");
        let vfs = bucket_vfs(&bucket, None);

        vfs.unlink("/data/only.txt").expect("unlink last child");
        assert_eq!(bucket.keys(), vec!["data/".to_owned()]);
        assert!(vfs.stat("/data").expect("directory survives").is_dir());
        assert!(vfs.readdir("/data").expect("listing").is_empty());
    }

    #[test]
    fn root_mutations_report_their_posix_errno() {
        let bucket = FakeBucket::new();
        let vfs = bucket_vfs(&bucket, None);
        assert_errno(vfs.mkdir("/"), Errno::EEXIST);
        assert_errno(vfs.unlink("/"), Errno::EISDIR);
        assert_errno(vfs.rmdir("/"), Errno::EBUSY);
        assert_errno(vfs.rename("/", "/x"), Errno::EINVAL);
        assert_errno(vfs.open("/", OpenMode::read_only()), Errno::EISDIR);
    }

    #[test]
    fn files_and_directories_rename_across_prefixes() {
        let bucket = FakeBucket::new();
        let vfs = bucket_vfs(&bucket, Some("root"));

        write_file(&vfs, "/a.txt", b"payload").expect("create");
        vfs.rename("/a.txt", "/b.txt").expect("rename file");
        assert_eq!(bucket.body("root/b.txt").as_deref(), Some(&b"payload"[..]));
        assert!(bucket.body("root/a.txt").is_none());
        vfs.rename("/b.txt", "/b.txt").expect("rename onto itself");
        assert_errno(vfs.rename("/missing", "/x"), Errno::ENOENT);

        vfs.mkdir("/tree").expect("mkdir");
        write_file(&vfs, "/tree/one.txt", b"1").expect("nested file");
        vfs.mkdir("/tree/inner").expect("nested directory");
        write_file(&vfs, "/tree/inner/two.txt", b"2").expect("deep file");

        vfs.rename("/tree", "/moved").expect("rename directory");
        assert_eq!(
            bucket.keys(),
            vec![
                "root/b.txt".to_owned(),
                "root/moved/".to_owned(),
                "root/moved/inner/".to_owned(),
                "root/moved/inner/two.txt".to_owned(),
                "root/moved/one.txt".to_owned(),
            ]
        );
        assert_eq!(
            read_file(&vfs, "/moved/inner/two.txt").expect("read moved file"),
            b"2"
        );
        assert_errno(vfs.stat("/tree"), Errno::ENOENT);
    }

    #[test]
    fn rename_target_conflicts_follow_posix() {
        let bucket = FakeBucket::new();
        let vfs = bucket_vfs(&bucket, None);

        write_file(&vfs, "/file.txt", b"file").expect("create file");
        write_file(&vfs, "/other.txt", b"other").expect("create other");
        vfs.mkdir("/dir").expect("mkdir");
        write_file(&vfs, "/dir/child.txt", b"child").expect("nested file");
        vfs.mkdir("/empty").expect("mkdir empty");

        assert_errno(vfs.rename("/file.txt", "/dir"), Errno::EISDIR);
        assert_errno(vfs.rename("/dir", "/file.txt"), Errno::ENOTDIR);
        assert_errno(vfs.rename("/empty", "/dir"), Errno::ENOTEMPTY);
        assert_errno(vfs.rename("/dir", "/dir/child"), Errno::EINVAL);

        // Replacing an existing file is allowed.
        vfs.rename("/file.txt", "/other.txt")
            .expect("replace target file");
        assert_eq!(bucket.body("other.txt").as_deref(), Some(&b"file"[..]));
    }

    #[test]
    fn directory_rename_can_be_disabled() {
        let bucket = FakeBucket::new();
        let vfs = configured_bucket_vfs(
            &bucket,
            None,
            S3VfsConfig {
                directory_rename: false,
                ..S3VfsConfig::default()
            },
        );

        vfs.mkdir("/dir").expect("mkdir");
        write_file(&vfs, "/dir/file.txt", b"data").expect("nested file");
        assert_errno(vfs.rename("/dir", "/moved"), Errno::EXDEV);
        vfs.rename("/dir/file.txt", "/dir/renamed.txt")
            .expect("file rename still works");
        assert_eq!(
            bucket.body("dir/renamed.txt").as_deref(),
            Some(&b"data"[..])
        );
    }

    #[test]
    fn open_modes_and_handle_errors_match_the_vfs_contract() {
        let bucket = FakeBucket::new();
        let vfs = bucket_vfs(&bucket, None);

        assert_errno(vfs.open("/missing", OpenMode::read_only()), Errno::ENOENT);
        assert_errno(vfs.open("/missing", OpenMode::default()), Errno::EINVAL);
        assert_errno(vfs.open("/missing", OpenMode::write_only()), Errno::ENOENT);

        let write_only = vfs
            .open("/file", OpenMode::write_only().create_new())
            .expect("exclusive create");
        assert_errno(vfs.read_at(write_only, 0, &mut [0; 1]), Errno::EBADF);
        assert_eq!(
            vfs.write_at(write_only, 0, b"").expect("empty write"),
            0,
            "an empty write succeeds at any offset"
        );
        vfs.write_at(write_only, 0, b"abc").expect("write");
        vfs.close(write_only).expect("close");

        let read_only = vfs.open("/file", OpenMode::read_only()).expect("reopen");
        assert_errno(vfs.write_at(read_only, 0, b"x"), Errno::EBADF);
        assert_errno(vfs.truncate(read_only, 0), Errno::EINVAL);
        vfs.close(read_only).expect("close");
        assert_errno(vfs.read_at(read_only, 0, &mut [0; 1]), Errno::EBADF);
        assert_errno(vfs.close(read_only), Errno::EBADF);

        write_file(&vfs, "/file", b"abc").expect("recreate");
        assert_errno(
            vfs.open("/file/child", OpenMode::read_only()),
            Errno::ENOTDIR,
        );
        assert_errno(
            vfs.open("/missing/file", OpenMode::write_only().create()),
            Errno::ENOENT,
        );
    }

    #[test]
    fn concurrent_handles_stage_independently_and_last_close_wins() {
        let bucket = FakeBucket::new();
        let vfs = configured_bucket_vfs(
            &bucket,
            None,
            S3VfsConfig {
                conditional_writes: false,
                ..S3VfsConfig::default()
            },
        );

        let first = vfs
            .open("/shared.txt", OpenMode::write_only().create().truncate())
            .expect("first handle");
        let second = vfs
            .open("/shared.txt", OpenMode::write_only().create().truncate())
            .expect("second handle");
        vfs.write_at(first, 0, b"first").expect("write first");
        vfs.write_at(second, 0, b"second").expect("write second");
        vfs.close(first).expect("close first");
        assert_eq!(bucket.body("shared.txt").as_deref(), Some(&b"first"[..]));
        vfs.close(second).expect("close second");
        assert_eq!(bucket.body("shared.txt").as_deref(), Some(&b"second"[..]));
    }

    #[test]
    fn writes_are_contained_by_the_configured_prefix() {
        let bucket = FakeBucket::new();
        bucket.seed("root-secret.txt", b"must remain invisible");
        let vfs = bucket_vfs(&bucket, Some("root"));

        write_file(&vfs, "/../escape.txt", b"contained").expect("escaping path is clamped");
        vfs.mkdir("/../dir").expect("escaping mkdir is clamped");
        assert_eq!(
            bucket.keys(),
            vec![
                "root-secret.txt".to_owned(),
                "root/dir/".to_owned(),
                "root/escape.txt".to_owned(),
            ]
        );
        assert_eq!(
            bucket.body("root-secret.txt").as_deref(),
            Some(&b"must remain invisible"[..])
        );
    }

    #[tokio::test]
    async fn shell_merged_redirects_share_one_s3_description_and_write_order() {
        use crate::sandbox::{CommandResult, Sandbox};
        use tokio::io::AsyncWriteExt;

        let bucket = FakeBucket::new();
        let sandbox = Sandbox::builder()
            .clear_mounts()
            .mount("objects", bucket_vfs(&bucket, None))
            .cwd("/objects")
            .command("abc", |mut ctx| async move {
                ctx.stdout.write_all(b"A").await.expect("stdout A");
                ctx.stderr.write_all(b"B").await.expect("stderr B");
                ctx.stdout.write_all(b"C").await.expect("stdout C");
                CommandResult::success()
            })
            .build();

        for command in ["abc > merged 2>&1", "echo input | abc > merged 2>&1"] {
            let result = sandbox.exec(command).await;
            assert_eq!(result.exit_code, 0, "{}", result.stderr);
            assert_eq!(bucket.body("merged").as_deref(), Some(&b"ABC"[..]));
        }
        assert_eq!(bucket.open_uploads(), 0);
    }

    #[tokio::test]
    async fn shell_replacement_redirect_streams_past_the_s3_edit_limit() {
        use crate::sandbox::{CommandResult, Sandbox};
        use tokio::io::AsyncWriteExt;

        let bucket = FakeBucket::new();
        bucket.seed("out", &[b'o'; 64]);
        let mut vfs = configured_bucket_vfs(
            &bucket,
            None,
            S3VfsConfig {
                max_edit_bytes: 16,
                ..S3VfsConfig::default()
            },
        );
        vfs.part_size = 8;
        let sandbox = Sandbox::builder()
            .clear_mounts()
            .mount("objects", vfs)
            .cwd("/objects")
            .command("payload", |mut ctx| async move {
                ctx.stdout
                    .write_all(&[b'x'; 31])
                    .await
                    .expect("stream replacement");
                CommandResult::success()
            })
            .build();

        let result = sandbox.exec("payload > out").await;
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(bucket.body("out"), Some(vec![b'x'; 31]));
        let result = sandbox
            .exec("echo value > superseded > final; > empty")
            .await;
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(bucket.body("superseded"), Some(Vec::new()));
        assert_eq!(bucket.body("empty"), Some(Vec::new()));
        assert_eq!(bucket.body("final").as_deref(), Some(&b"value\n"[..]));
        assert_eq!(bucket.open_uploads(), 0);
    }

    #[test]
    fn explicit_abort_discards_buffered_edits_and_pending_multipart_uploads() {
        let bucket = FakeBucket::new();
        bucket.seed("existing", b"original");
        let mut vfs = bucket_vfs(&bucket, None);
        vfs.part_size = 8;

        let edit = vfs
            .open("/existing", OpenMode::read_write())
            .expect("open edit");
        vfs.write_at(edit, 0, b"changed!")
            .expect("stage edited body");
        vfs.abort(edit).expect("discard edit");
        assert_eq!(bucket.body("existing").as_deref(), Some(&b"original"[..]));
        assert_errno(vfs.close(edit), Errno::EBADF);

        let buffered = vfs
            .open("/new-buffer", OpenMode::write_only().create().truncate())
            .expect("open buffered stream");
        vfs.write_at(buffered, 0, b"new")
            .expect("stage unflushed bytes");
        vfs.abort(buffered).expect("discard unflushed bytes");
        assert!(bucket.body("new-buffer").is_none());

        let multipart = vfs
            .open("/existing", OpenMode::write_only().truncate())
            .expect("open replacement stream");
        vfs.write_at(multipart, 0, &[b'x'; 24])
            .expect("upload parts");
        assert_eq!(bucket.open_uploads(), 1);
        vfs.abort(multipart).expect("abort uploaded parts");
        assert_eq!(bucket.body("existing").as_deref(), Some(&b"original"[..]));
        assert_eq!(bucket.open_uploads(), 0);
        assert_eq!(bucket.aborted().len(), 1);
        assert!(vfs.state().handles.is_empty());
    }

    #[tokio::test]
    async fn cancelled_redirects_abort_s3_staging_and_preserve_existing_objects() {
        use crate::sandbox::{CommandResult, Limits, Sandbox};
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;

        for drop_future in [false, true] {
            let bucket = FakeBucket::new();
            bucket.seed("existing", b"original");
            let mut vfs = bucket_vfs(&bucket, None);
            vfs.part_size = 8;
            let vfs = Arc::new(vfs);
            let staged = Arc::new(tokio::sync::Notify::new());
            let notify = Arc::clone(&staged);
            let sandbox = Sandbox::builder()
                .clear_mounts()
                .mount_arc("objects", Arc::clone(&vfs) as Arc<dyn Vfs>)
                .cwd("/objects")
                .limits(Limits {
                    wall_time: Duration::from_millis(500),
                    ..Limits::default()
                })
                .command("stage", move |mut ctx| {
                    let notify = Arc::clone(&notify);
                    async move {
                        ctx.stdout
                            .write_all(&[b'x'; 24])
                            .await
                            .expect("stage multipart output");
                        ctx.stdout
                            .flush()
                            .await
                            .expect("flush staged multipart output");
                        notify.notify_one();
                        std::future::pending::<CommandResult>().await
                    }
                })
                .build();
            let mut exec = Box::pin(sandbox.exec("stage > existing"));
            tokio::select! {
                result = &mut exec => panic!("exec ended before output was staged: {result:?}"),
                _ = staged.notified() => {}
            }
            assert_eq!(bucket.open_uploads(), 1);
            if drop_future {
                drop(exec);
            } else {
                assert_eq!(exec.await.exit_code, 124);
            }
            // Cleanup may finish after the caller regains control. Bound this
            // assertion independently of the execution timeout being tested.
            tokio::time::timeout(Duration::from_secs(3), async {
                while !vfs.state().handles.is_empty() || bucket.open_uploads() != 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("all abandoned S3 handles cleaned up");
            assert_eq!(bucket.body("existing").as_deref(), Some(&b"original"[..]));
            assert_eq!(bucket.aborted().len(), 1);
        }
    }
}
