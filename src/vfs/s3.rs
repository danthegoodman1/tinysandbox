//! Read-only S3-backed virtual filesystem.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};

use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use tokio::io::{AsyncRead, AsyncReadExt};

use super::path::normalize_path;
use super::{DirEntry, Errno, FileHandle, FileType, Metadata, OpenMode, Vfs, VfsError, VfsResult};

/// A read-only view of one S3 bucket prefix.
///
/// The supplied client owns endpoint, credentials, retry, timeout, TLS, region,
/// and path-style policy. Construction performs no network I/O.
pub struct S3Vfs {
    ops: Arc<dyn S3Ops>,
    bucket: String,
    root_prefix: String,
    state: Mutex<State>,
}

impl std::fmt::Debug for S3Vfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Vfs")
            .field("bucket", &self.bucket)
            .field("root_prefix", &self.root_prefix)
            .finish_non_exhaustive()
    }
}

impl S3Vfs {
    /// Creates a read-only filesystem rooted at `prefix` in `bucket`.
    ///
    /// Leading and trailing slashes in the prefix are normalized. Empty,
    /// current-directory, parent-directory, and NUL-containing components are
    /// rejected. The empty prefix exposes the whole bucket.
    pub fn new(
        client: aws_sdk_s3::Client,
        bucket: impl Into<String>,
        prefix: Option<&str>,
    ) -> VfsResult<Self> {
        Self::with_ops(Arc::new(AwsS3Ops { client }), bucket.into(), prefix)
    }

    fn with_ops(ops: Arc<dyn S3Ops>, bucket: String, prefix: Option<&str>) -> VfsResult<Self> {
        validate_bucket(&bucket)?;
        let root_prefix = normalize_prefix(prefix.unwrap_or_default())?;
        Ok(Self {
            ops,
            bucket,
            root_prefix,
            state: Mutex::new(State {
                next_handle: 1,
                handles: BTreeMap::new(),
            }),
        })
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

    fn validate_mutation_path(&self, path: &str) -> VfsResult<()> {
        self.components(path).map(|_| ())
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
        self.validate_mutation_path(path)?;
        Err(vfs_error(Errno::EACCES))
    }

    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        self.validate_mutation_path(from)?;
        self.validate_mutation_path(to)?;
        Err(vfs_error(Errno::EACCES))
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        self.validate_mutation_path(path)?;
        Err(vfs_error(Errno::EACCES))
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        self.validate_mutation_path(path)?;
        Err(vfs_error(Errno::EACCES))
    }

    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        if mode != OpenMode::read_only() {
            if !mode.read
                && !mode.write
                && !mode.create
                && !mode.create_new
                && !mode.truncate
                && !mode.append
            {
                return Err(vfs_error(Errno::EINVAL));
            }
            return Err(vfs_error(Errno::EACCES));
        }

        let components = self.components(path)?;
        self.validate_parents(&components)?;
        let file = match self.kind_at(&components)? {
            Some(Kind::Directory(_)) => return Err(vfs_error(Errno::EISDIR)),
            Some(Kind::File(file)) => file,
            None => return Err(vfs_error(Errno::ENOENT)),
        };

        let mut state = self.state();
        let handle = FileHandle::new(state.next_handle);
        state.next_handle = state
            .next_handle
            .checked_add(1)
            .ok_or(vfs_error(Errno::EIO))?;
        state.handles.insert(
            handle,
            HandleState {
                key: self.key(&components),
                len: file.len,
                etag: file.etag,
            },
        );
        Ok(handle)
    }

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let open = self
            .state()
            .handles
            .get(&handle)
            .cloned()
            .ok_or(vfs_error(Errno::EBADF))?;
        if buf.is_empty() || offset >= open.len {
            return Ok(0);
        }

        let remaining = open.len - offset;
        let requested = remaining.min(u64::try_from(buf.len()).map_err(|_| vfs_error(Errno::EIO))?);
        let end = offset
            .checked_add(requested)
            .and_then(|value| value.checked_sub(1))
            .ok_or(vfs_error(Errno::EIO))?;
        let range = format!("bytes={offset}-{end}");
        let expected_content_range = format!("bytes {offset}-{end}/{}", open.len);
        let response = self
            .ops
            .get(GetRequest {
                bucket: self.bucket.clone(),
                key: open.key,
                range,
                if_match: open.etag.clone(),
                expected_len: requested,
                expected_content_range: expected_content_range.clone(),
            })
            .map_err(RemoteError::into_vfs)?;

        let expected = usize::try_from(requested).map_err(|_| vfs_error(Errno::EIO))?;
        if response.body.len() != expected
            || response.content_length != requested
            || response.etag.as_deref() != Some(open.etag.as_str())
            || response.content_range.as_deref() != Some(expected_content_range.as_str())
        {
            return Err(vfs_error(Errno::EIO));
        }
        buf[..expected].copy_from_slice(&response.body);
        Ok(expected)
    }

    fn write_at(&self, handle: FileHandle, _offset: u64, _data: &[u8]) -> VfsResult<usize> {
        if !self.state().handles.contains_key(&handle) {
            return Err(vfs_error(Errno::EBADF));
        }
        Err(vfs_error(Errno::EBADF))
    }

    fn truncate(&self, handle: FileHandle, _len: u64) -> VfsResult<()> {
        if !self.state().handles.contains_key(&handle) {
            return Err(vfs_error(Errno::EBADF));
        }
        Err(vfs_error(Errno::EINVAL))
    }

    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        self.state()
            .handles
            .remove(&handle)
            .map(|_| ())
            .ok_or(vfs_error(Errno::EBADF))
    }
}

#[derive(Debug)]
struct State {
    next_handle: u64,
    handles: BTreeMap<FileHandle, HandleState>,
}

#[derive(Debug, Clone)]
struct HandleState {
    key: String,
    len: u64,
    etag: String,
}

#[derive(Debug)]
enum Kind {
    Directory(Vec<DirEntry>),
    File(HeadResult),
}

#[derive(Debug)]
struct DirectoryListing {
    exists: bool,
    entries: Vec<DirEntry>,
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
    delimiter: String,
    continuation: Option<String>,
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
struct HeadResult {
    len: u64,
    etag: String,
}

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
            let output = client
                .list_objects_v2()
                .bucket(request.bucket)
                .prefix(request.prefix)
                .delimiter(request.delimiter)
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
        (Some("PreconditionFailed" | "412"), _) | (_, Some(412)) => RemoteError::Precondition,
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
    }

    fn list_request(prefix: &str, continuation: Option<&str>) -> ListRequest {
        ListRequest {
            bucket: "bucket".to_owned(),
            prefix: prefix.to_owned(),
            delimiter: "/".to_owned(),
            continuation: continuation.map(str::to_owned),
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
        S3Vfs::with_ops(fake, "bucket".to_owned(), prefix).expect("valid fake VFS")
    }

    fn assert_errno<T>(result: VfsResult<T>, errno: Errno) {
        match result {
            Ok(_) => panic!("operation should fail"),
            Err(error) => assert_eq!(error.errno(), errno),
        }
    }

    #[test]
    fn construction_normalizes_prefix_and_rejects_invalid_configuration() {
        let fake = FakeOps::new(vec![]);
        let rooted = vfs(Arc::clone(&fake), Some("/team/data/"));
        assert_eq!(rooted.root_prefix, "team/data/");
        assert_eq!(vfs(Arc::clone(&fake), Some("///")).root_prefix, "");
        assert_errno(
            S3Vfs::with_ops(Arc::clone(&fake) as Arc<dyn S3Ops>, "".into(), None),
            Errno::EINVAL,
        );
        for prefix in ["a//b", "a/./b", "a/../b", "a\0b"] {
            assert_errno(
                S3Vfs::with_ops(
                    Arc::clone(&fake) as Arc<dyn S3Ops>,
                    "bucket".into(),
                    Some(prefix),
                ),
                Errno::EINVAL,
            );
        }
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
    fn read_only_and_handle_errors_are_stable() {
        let fake = FakeOps::new(vec![
            Call::List(list_request("file/", None), Ok(empty_list())),
            file_head("file", 1, "e"),
        ]);
        let vfs = vfs(Arc::clone(&fake), None);
        assert!(!vfs.is_fast());
        assert!(vfs.stats().is_none());
        assert_errno(vfs.open("/file", OpenMode::default()), Errno::EINVAL);
        for mode in [
            OpenMode::write_only(),
            OpenMode::read_write(),
            OpenMode::read_only().create(),
            OpenMode::read_only().create_new(),
            OpenMode::read_only().truncate(),
            OpenMode::read_only().append(),
        ] {
            assert_errno(vfs.open("/file", mode), Errno::EACCES);
        }
        for result in [
            vfs.mkdir("/x"),
            vfs.unlink("/x"),
            vfs.rmdir("/x"),
            vfs.rename("/x", "/y"),
        ] {
            assert_errno(result, Errno::EACCES);
        }
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
        for (code, status, expected) in [
            (Some("NoSuchKey"), None, RemoteError::Missing),
            (Some("NoSuchBucket"), None, RemoteError::Missing),
            (None, Some(404), RemoteError::Missing),
            (Some("AccessDenied"), None, RemoteError::Denied),
            (None, Some(401), RemoteError::Denied),
            (None, Some(403), RemoteError::Denied),
            (None, Some(412), RemoteError::Precondition),
            (None, Some(500), RemoteError::Io),
            (None, None, RemoteError::Io),
        ] {
            assert_eq!(classify_remote(code, status), expected);
        }
    }

    #[test]
    fn direct_s3_vfs_calls_are_safe_inside_tokio_and_concurrently() {
        let vfs = Arc::new(
            S3Vfs::with_ops(Arc::new(BridgedOps), "bucket".into(), None)
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
}
