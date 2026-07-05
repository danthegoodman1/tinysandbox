use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use napi::bindgen_prelude::{
    AsyncTask, Buffer, External, FromNapiValue, Function, JsObjectValue, Object, Promise,
};
use napi::threadsafe_function::ThreadsafeFunction;
use napi::{Error, JsExternal, Result, Status, Task};
use napi_derive::napi;
use serde_json::Value;
use tinysandbox::sandbox::{
    Command, CommandContext, CommandFuture, CommandResult, ExecResult as CoreExecResult,
    FetchRequest as CoreFetchRequest, FetchResponse as CoreFetchResponse, Limits,
    Sandbox as CoreSandbox, SyscallError,
};
use tinysandbox::vfs::{
    DirEntry, Errno, FileHandle, FileType, Metadata, OpenMode, Vfs, VfsError, VfsQuota, VfsResult,
    VfsStats,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type JsCommandCallback = Arc<
    ThreadsafeFunction<CommandCall, Promise<CommandOutput>, (CommandCall,), Status, false, true>,
>;
type JsSyscallCallback =
    Arc<ThreadsafeFunction<Value, Promise<SyscallCallbackResponse>, (Value,), Status, false, true>>;
type JsFetchCallback = Arc<
    ThreadsafeFunction<
        FetchRequest,
        Promise<FetchCallbackResponse>,
        (FetchRequest,),
        Status,
        false,
        true,
    >,
>;
type JsVfsCallback =
    Arc<ThreadsafeFunction<VfsRequest, Promise<VfsResponse>, (VfsRequest,), Status, false, true>>;
type JsVfsFactoryCallback =
    Arc<ThreadsafeFunction<VfsQuotaJs, Promise<JsVfsHandle>, (VfsQuotaJs,), Status, false, true>>;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[napi(js_name = "NativeSandbox")]
pub struct Sandbox {
    // Napi owns this Arc through the JS object finalizer, so the final drop of
    // JsVfs runtimes stays outside Tokio async contexts.
    inner: Arc<CoreSandbox>,
}

#[napi]
impl Sandbox {
    #[napi(constructor)]
    pub fn new(options: Option<Object<'_>>) -> Result<Self> {
        let mut builder = CoreSandbox::builder();

        if let Some(options) = options {
            if let Some(limits) = get_optional_object(&options, "limits")? {
                builder = builder.limits(parse_limits(limits)?);
            }
            if let Some(env) = get_optional_object(&options, "env")? {
                for key in Object::keys(&env)? {
                    let value: String = env.get_named_property(&key)?;
                    builder = builder.env(key, value);
                }
            }
            if let Some(cwd) = get_optional::<String>(&options, "cwd")? {
                builder = builder.cwd(cwd);
            }
            if let Some(persist) = get_optional::<bool>(&options, "persistSession")? {
                builder = builder.persist_session(persist);
            }
            if let Some(syscalls) = get_optional_object(&options, "syscalls")? {
                for name in Object::keys(&syscalls)? {
                    validate_syscall_name(&name)?;
                    let callback: Function<'_, (Value,), Promise<SyscallCallbackResponse>> =
                        syscalls.get_named_property(&name)?;
                    let callback = Arc::new(
                        callback
                            .build_threadsafe_function::<Value>()
                            .callee_handled::<false>()
                            .weak::<true>()
                            .build_callback(|ctx| Ok((ctx.value,)))?,
                    );
                    builder = builder.syscall(name, move |args| {
                        let callback = Arc::clone(&callback);
                        async move { call_js_syscall(callback, args).await }
                    });
                }
            }
            if let Some(js_prelude) = get_optional::<String>(&options, "jsPrelude")? {
                builder = builder.js_prelude(js_prelude);
            }
            if options.has_named_property("fetch")? {
                let fetch: Function<'_, (FetchRequest,), Promise<FetchCallbackResponse>> =
                    options.get_named_property("fetch")?;
                let callback = Arc::new(
                    fetch
                        .build_threadsafe_function::<FetchRequest>()
                        .callee_handled::<false>()
                        .weak::<true>()
                        .build_callback(|ctx| Ok((ctx.value,)))?,
                );
                builder = builder.fetch(move |request| {
                    let callback = Arc::clone(&callback);
                    async move { call_js_fetch(callback, request).await }
                });
            }
            if let Some(vfs) = get_optional_object(&options, "vfs")? {
                builder = builder.vfs_arc(Arc::new(JsVfs::new(vfs)?));
            }
            if let Some(commands) = get_optional_object(&options, "commands")? {
                for name in Object::keys(&commands)? {
                    let callback: Function<'_, (CommandCall,), Promise<CommandOutput>> =
                        commands.get_named_property(&name)?;
                    let callback = callback
                        .build_threadsafe_function::<CommandCall>()
                        .callee_handled::<false>()
                        .weak::<true>()
                        .build_callback(|ctx| Ok((ctx.value,)))?;
                    builder = builder.command_obj(
                        name,
                        JsCommand {
                            callback: Arc::new(callback),
                        },
                    );
                }
            }
        }

        Ok(Self {
            inner: Arc::new(builder.build()),
        })
    }

    #[napi]
    pub async fn exec(&self, script: String) -> ExecResult {
        CoreExecResult::into(self.inner.exec(&script).await)
    }

    #[napi(getter)]
    pub fn fs(&self) -> SandboxFs {
        SandboxFs {
            sandbox: Arc::clone(&self.inner),
        }
    }

    #[napi]
    pub async fn stats(&self) -> Result<SandboxStats> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let stats = inner.stats();
            SandboxStats {
                commands_run: stats.commands_run as f64,
                vfs: stats.vfs.map(VfsStatsJs::from),
            }
        })
        .await
        .map_err(|err| Error::new(Status::GenericFailure, err.to_string()))
    }
}

#[napi]
pub struct SandboxFs {
    sandbox: Arc<CoreSandbox>,
}

#[napi]
impl SandboxFs {
    #[napi]
    pub async fn stat(&self, path: String) -> Result<FileStat> {
        let fs = self.sandbox.fs();
        fs.stat(&path)
            .await
            .map(FileStat::from)
            .map_err(|err| napi_vfs_error(err, Some(&path)))
    }

    #[napi]
    pub async fn readdir(&self, path: String) -> Result<Vec<DirEntryJs>> {
        let fs = self.sandbox.fs();
        fs.readdir(&path)
            .await
            .map(|entries| entries.into_iter().map(DirEntryJs::from).collect())
            .map_err(|err| napi_vfs_error(err, Some(&path)))
    }

    #[napi]
    pub async fn mkdir(&self, path: String) -> Result<()> {
        let fs = self.sandbox.fs();
        fs.mkdir(&path)
            .await
            .map_err(|err| napi_vfs_error(err, Some(&path)))
    }

    #[napi]
    pub async fn rename(&self, from: String, to: String) -> Result<()> {
        let fs = self.sandbox.fs();
        fs.rename(&from, &to)
            .await
            .map_err(|err| napi_vfs_error(err, Some(&from)))
    }

    #[napi]
    pub async fn unlink(&self, path: String) -> Result<()> {
        let fs = self.sandbox.fs();
        fs.unlink(&path)
            .await
            .map_err(|err| napi_vfs_error(err, Some(&path)))
    }

    #[napi]
    pub async fn rmdir(&self, path: String) -> Result<()> {
        let fs = self.sandbox.fs();
        fs.rmdir(&path)
            .await
            .map_err(|err| napi_vfs_error(err, Some(&path)))
    }

    #[napi]
    pub async fn read_file(&self, path: String) -> Result<Buffer> {
        let fs = self.sandbox.fs();
        fs.read_file(&path)
            .await
            .map(Buffer::from)
            .map_err(|err| napi_vfs_error(err, Some(&path)))
    }

    #[napi]
    pub async fn write_file(&self, path: String, data: Buffer) -> Result<()> {
        let fs = self.sandbox.fs();
        fs.write_file(&path, &data, false)
            .await
            .map_err(|err| napi_vfs_error(err, Some(&path)))
    }

    #[napi]
    pub async fn append_file(&self, path: String, data: Buffer) -> Result<()> {
        let fs = self.sandbox.fs();
        fs.write_file(&path, &data, true)
            .await
            .map_err(|err| napi_vfs_error(err, Some(&path)))
    }

    #[napi]
    pub async fn open(&self, path: String, mode: OpenModeJs) -> Result<f64> {
        let fs = self.sandbox.fs();
        fs.open(&path, OpenMode::from(mode))
            .await
            .map(|handle| handle.raw() as f64)
            .map_err(|err| napi_vfs_error(err, Some(&path)))
    }

    #[napi]
    pub async fn read_at(&self, handle: f64, offset: f64, len: f64) -> Result<Buffer> {
        let handle = handle_from_js(handle).map_err(|err| napi_vfs_error(err, None))?;
        let offset = u64_from_js(offset).map_err(|err| napi_vfs_error(err, None))?;
        let len = usize_from_js(len).map_err(|err| napi_vfs_error(err, None))?;
        let fs = self.sandbox.fs();
        fs.read_at(handle, offset, vec![0; len])
            .await
            .map(|(mut data, read)| {
                data.truncate(read);
                Buffer::from(data)
            })
            .map_err(|err| napi_vfs_error(err, None))
    }

    #[napi]
    pub async fn write_at(&self, handle: f64, offset: f64, data: Buffer) -> Result<f64> {
        let handle = handle_from_js(handle).map_err(|err| napi_vfs_error(err, None))?;
        let offset = u64_from_js(offset).map_err(|err| napi_vfs_error(err, None))?;
        let fs = self.sandbox.fs();
        fs.write_at(handle, offset, data.to_vec())
            .await
            .map(|written| written as f64)
            .map_err(|err| napi_vfs_error(err, None))
    }

    #[napi]
    pub async fn truncate(&self, handle: f64, len: f64) -> Result<()> {
        let handle = handle_from_js(handle).map_err(|err| napi_vfs_error(err, None))?;
        let len = u64_from_js(len).map_err(|err| napi_vfs_error(err, None))?;
        let fs = self.sandbox.fs();
        fs.truncate(handle, len)
            .await
            .map_err(|err| napi_vfs_error(err, None))
    }

    #[napi]
    pub async fn close(&self, handle: f64) -> Result<()> {
        let handle = handle_from_js(handle).map_err(|err| napi_vfs_error(err, None))?;
        let fs = self.sandbox.fs();
        fs.close(handle)
            .await
            .map_err(|err| napi_vfs_error(err, None))
    }
}

#[derive(Clone)]
struct JsCommand {
    callback: JsCommandCallback,
}

impl Command for JsCommand {
    fn run(&self, mut ctx: CommandContext) -> CommandFuture {
        let callback = Arc::clone(&self.callback);
        Box::pin(async move {
            let mut stdin = Vec::new();
            if ctx.stdin.read_to_end(&mut stdin).await.is_err() {
                return CommandResult::failure();
            }

            let call = CommandCall {
                args: ctx.args,
                env: ctx.env.into_iter().collect(),
                cwd: ctx.cwd,
                stdin: Buffer::from(stdin),
            };

            let output = match callback.call_async_catch(call).await {
                Ok(promise) => match promise.await {
                    Ok(output) => output,
                    Err(err) => return write_command_error(ctx.stderr, err.reason).await,
                },
                Err(err) => return write_command_error(ctx.stderr, err.reason).await,
            };

            if let Some(stdout) = output.stdout
                && ctx.stdout.write_all(&stdout).await.is_err()
            {
                return CommandResult::failure();
            }
            if let Some(stderr) = output.stderr
                && ctx.stderr.write_all(&stderr).await.is_err()
            {
                return CommandResult::failure();
            }
            CommandResult::new(output.exit_code.unwrap_or(0))
        })
    }
}

async fn write_command_error(
    mut stderr: tinysandbox::sandbox::BoxAsyncWrite,
    reason: String,
) -> CommandResult {
    let _ = stderr
        .write_all(format!("tinysandbox-node: custom command failed: {reason}\n").as_bytes())
        .await;
    CommandResult::failure()
}

async fn call_js_syscall(
    callback: JsSyscallCallback,
    args: Value,
) -> std::result::Result<Value, SyscallError> {
    let promise = callback
        .call_async_catch(args)
        .await
        .map_err(|err| SyscallError::new(err.reason))?;
    let response = promise.await.map_err(|err| SyscallError::new(err.reason))?;
    if let Some(error) = response.error {
        return Err(syscall_error_from_callback(error));
    }
    Ok(response.value.unwrap_or(Value::Null))
}

async fn call_js_fetch(
    callback: JsFetchCallback,
    request: CoreFetchRequest,
) -> std::result::Result<CoreFetchResponse, SyscallError> {
    let promise = callback
        .call_async_catch(FetchRequest::from(request))
        .await
        .map_err(|err| SyscallError::new(err.reason))?;
    let response = promise.await.map_err(|err| SyscallError::new(err.reason))?;
    if let Some(error) = response.error {
        return Err(syscall_error_from_callback(error));
    }
    let response = response
        .response
        .ok_or_else(|| SyscallError::new("fetch handler did not return a response"))?;
    Ok(CoreFetchResponse {
        status: status_from_js(response.status)?,
        headers: header_pairs_from_js(response.headers.unwrap_or_default())?,
        body: response.body.map(|body| body.to_vec()).unwrap_or_default(),
    })
}

fn syscall_error_from_callback(error: SyscallCallbackError) -> SyscallError {
    let message = error
        .message
        .unwrap_or_else(|| "host callback failed".to_owned());
    match error.code {
        Some(code) => SyscallError::new(message).with_code(code),
        None => SyscallError::new(message),
    }
}

fn status_from_js(status: Option<f64>) -> std::result::Result<u16, SyscallError> {
    let status = status.ok_or_else(|| SyscallError::new("fetch response status is required"))?;
    if status.is_finite() && status.fract() == 0.0 && (100.0..=599.0).contains(&status) {
        Ok(status as u16)
    } else {
        Err(SyscallError::new(
            "fetch response status must be an integer from 100 through 599",
        ))
    }
}

fn header_pairs_from_js(
    headers: Vec<Vec<String>>,
) -> std::result::Result<Vec<(String, String)>, SyscallError> {
    headers
        .into_iter()
        .map(|pair| match pair.as_slice() {
            [name, value] => Ok((name.clone(), value.clone())),
            _ => Err(SyscallError::new(
                "fetch response headers must be [name, value] pairs",
            )),
        })
        .collect()
}

struct JsVfs {
    callbacks: HashMap<&'static str, JsVfsCallback>,
    runtime: tokio::runtime::Runtime,
}

impl JsVfs {
    fn new(vfs: Object<'_>) -> Result<Self> {
        let mut callbacks = HashMap::new();
        for name in [
            "stat", "readdir", "mkdir", "rename", "unlink", "rmdir", "open", "readAt", "writeAt",
            "truncate", "close",
        ] {
            callbacks.insert(name, vfs_callback(&vfs, name)?);
        }
        if vfs.has_named_property("stats")? {
            callbacks.insert("stats", vfs_callback(&vfs, "stats")?);
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|err| Error::new(Status::GenericFailure, err.to_string()))?;

        Ok(Self { callbacks, runtime })
    }

    fn call(&self, name: &'static str, request: VfsRequest) -> VfsResult<VfsResponse> {
        let callback = self
            .callbacks
            .get(name)
            .ok_or(VfsError::new(Errno::EINVAL))?;
        let response = self
            .runtime
            .block_on(async {
                let promise = callback
                    .call_async_catch(request)
                    .await
                    .map_err(|_| Errno::EINVAL)?;
                promise.await.map_err(|_| Errno::EINVAL)
            })
            .map_err(VfsError::new)?;

        if let Some(error) = response.error {
            return Err(VfsError::new(errno_from_code(error.code.as_deref())));
        }

        Ok(response)
    }
}

impl Vfs for JsVfs {
    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        response_metadata(self.call("stat", VfsRequest::path(path))?)
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        let response = self.call("readdir", VfsRequest::path(path))?;
        response
            .entries
            .ok_or(VfsError::new(Errno::EINVAL))?
            .into_iter()
            .map(DirEntry::try_from)
            .collect()
    }

    fn mkdir(&self, path: &str) -> VfsResult<()> {
        self.call("mkdir", VfsRequest::path(path)).map(drop)
    }

    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        self.call(
            "rename",
            VfsRequest {
                from: Some(from.to_owned()),
                to: Some(to.to_owned()),
                ..VfsRequest::default()
            },
        )
        .map(drop)
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        self.call("unlink", VfsRequest::path(path)).map(drop)
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        self.call("rmdir", VfsRequest::path(path)).map(drop)
    }

    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        let response = self.call(
            "open",
            VfsRequest {
                path: Some(path.to_owned()),
                mode: Some(OpenModeJs::from(mode)),
                ..VfsRequest::default()
            },
        )?;
        response
            .handle
            .ok_or(VfsError::new(Errno::EINVAL))
            .and_then(|handle| u64_from_js(handle).map(FileHandle::new))
    }

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let response = self.call(
            "readAt",
            VfsRequest {
                handle: Some(handle.raw() as f64),
                offset: Some(offset as f64),
                len: Some(buf.len() as f64),
                ..VfsRequest::default()
            },
        )?;
        let data = response.data.ok_or(VfsError::new(Errno::EINVAL))?;
        let read = match response.bytes_read {
            Some(read) => f64_to_usize_lossless(read)?,
            None => data.len(),
        };
        let copy_len = read.min(data.len()).min(buf.len());
        buf[..copy_len].copy_from_slice(&data[..copy_len]);
        Ok(copy_len)
    }

    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        let response = self.call(
            "writeAt",
            VfsRequest {
                handle: Some(handle.raw() as f64),
                offset: Some(offset as f64),
                data: Some(Buffer::from(data.to_vec())),
                ..VfsRequest::default()
            },
        )?;
        response
            .bytes_written
            .map(f64_to_usize_lossless)
            .unwrap_or(Ok(data.len()))
    }

    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        self.call(
            "truncate",
            VfsRequest {
                handle: Some(handle.raw() as f64),
                len: Some(len as f64),
                ..VfsRequest::default()
            },
        )
        .map(drop)
    }

    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        self.call(
            "close",
            VfsRequest {
                handle: Some(handle.raw() as f64),
                ..VfsRequest::default()
            },
        )
        .map(drop)
    }

    fn stats(&self) -> Option<VfsResult<VfsStats>> {
        let _ = self.callbacks.get("stats")?;
        Some(
            self.call("stats", VfsRequest::default())
                .and_then(response_stats),
        )
    }
}

pub struct JsVfsExternal {
    inner: Arc<JsVfs>,
}

pub struct JsVfsHandle {
    inner: Arc<JsVfs>,
}

impl FromNapiValue for JsVfsHandle {
    unsafe fn from_napi_value(
        env: napi::sys::napi_env,
        napi_val: napi::sys::napi_value,
    ) -> Result<Self> {
        let external = unsafe { JsExternal::from_napi_value(env, napi_val)? };
        Ok(Self {
            inner: Arc::clone(&external.get_value::<JsVfsExternal>()?.inner),
        })
    }
}

impl Vfs for JsVfsHandle {
    fn stat(&self, path: &str) -> VfsResult<Metadata> {
        self.inner.stat(path)
    }

    fn readdir(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        self.inner.readdir(path)
    }

    fn mkdir(&self, path: &str) -> VfsResult<()> {
        self.inner.mkdir(path)
    }

    fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        self.inner.rename(from, to)
    }

    fn unlink(&self, path: &str) -> VfsResult<()> {
        self.inner.unlink(path)
    }

    fn rmdir(&self, path: &str) -> VfsResult<()> {
        self.inner.rmdir(path)
    }

    fn open(&self, path: &str, mode: OpenMode) -> VfsResult<FileHandle> {
        self.inner.open(path, mode)
    }

    fn read_at(&self, handle: FileHandle, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.inner.read_at(handle, offset, buf)
    }

    fn write_at(&self, handle: FileHandle, offset: u64, data: &[u8]) -> VfsResult<usize> {
        self.inner.write_at(handle, offset, data)
    }

    fn truncate(&self, handle: FileHandle, len: u64) -> VfsResult<()> {
        self.inner.truncate(handle, len)
    }

    fn close(&self, handle: FileHandle) -> VfsResult<()> {
        self.inner.close(handle)
    }

    fn stats(&self) -> Option<VfsResult<VfsStats>> {
        self.inner.stats()
    }
}

#[napi]
pub fn create_js_vfs(vfs: Object<'_>) -> Result<External<JsVfsExternal>> {
    Ok(External::new(JsVfsExternal {
        inner: Arc::new(JsVfs::new(vfs)?),
    }))
}

struct JsVfsFactory {
    callback: JsVfsFactoryCallback,
    runtime: tokio::runtime::Runtime,
}

impl JsVfsFactory {
    fn new(factory: Function<'_, (VfsQuotaJs,), Promise<JsVfsHandle>>) -> Result<Self> {
        let callback = factory
            .build_threadsafe_function::<VfsQuotaJs>()
            .callee_handled::<false>()
            .weak::<true>()
            .build_callback(|ctx| Ok((ctx.value,)))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|err| Error::new(Status::GenericFailure, err.to_string()))?;
        Ok(Self {
            callback: Arc::new(callback),
            runtime,
        })
    }

    fn create(&self, quota: VfsQuota) -> VfsResult<JsVfsHandle> {
        self.runtime
            .block_on(async {
                let promise = self
                    .callback
                    .call_async_catch(VfsQuotaJs::from(quota))
                    .await
                    .map_err(|_| Errno::EINVAL)?;
                promise.await.map_err(|_| Errno::EINVAL)
            })
            .map_err(VfsError::new)
    }
}

pub struct ConformanceTask {
    factory: JsVfsFactory,
}

impl Task for ConformanceTask {
    type Output = ConformanceResult;
    type JsValue = ConformanceResult;

    fn compute(&mut self) -> Result<Self::Output> {
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            tinysandbox::vfs::conformance::run(|quota| match self.factory.create(quota) {
                Ok(vfs) => vfs,
                Err(err) => panic!("JS VFS factory failed: {err}"),
            });
        }));

        match result {
            Ok(()) => Ok(ConformanceResult {
                ok: true,
                snapshots: "unsupported".to_owned(),
            }),
            Err(payload) => Err(Error::new(
                Status::GenericFailure,
                panic_message(payload.as_ref()),
            )),
        }
    }

    fn resolve(&mut self, _env: napi::Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

#[napi]
pub fn run_conformance(
    factory: Function<'_, (VfsQuotaJs,), Promise<JsVfsHandle>>,
) -> Result<AsyncTask<ConformanceTask>> {
    Ok(AsyncTask::new(ConformanceTask {
        factory: JsVfsFactory::new(factory)?,
    }))
}

#[napi(object)]
pub struct CommandCall {
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: String,
    pub stdin: Buffer,
}

#[napi(object)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: Option<Buffer>,
    pub stderr: Option<Buffer>,
}

#[napi(object)]
pub struct SyscallCallbackResponse {
    pub value: Option<Value>,
    pub error: Option<SyscallCallbackError>,
}

#[napi(object)]
pub struct SyscallCallbackError {
    pub message: Option<String>,
    pub code: Option<String>,
}

#[napi(object)]
pub struct FetchRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<Vec<String>>,
    pub body: Option<Buffer>,
}

impl From<CoreFetchRequest> for FetchRequest {
    fn from(request: CoreFetchRequest) -> Self {
        Self {
            url: request.url,
            method: request.method,
            headers: request
                .headers
                .into_iter()
                .map(|(name, value)| vec![name, value])
                .collect(),
            body: request.body.map(Buffer::from),
        }
    }
}

#[napi(object)]
pub struct FetchCallbackResponse {
    pub response: Option<FetchResponse>,
    pub error: Option<SyscallCallbackError>,
}

#[napi(object)]
pub struct FetchResponse {
    pub status: Option<f64>,
    pub headers: Option<Vec<Vec<String>>>,
    pub body: Option<Buffer>,
}

#[napi(object)]
#[derive(Default)]
pub struct VfsRequest {
    pub path: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub mode: Option<OpenModeJs>,
    pub handle: Option<f64>,
    pub offset: Option<f64>,
    pub len: Option<f64>,
    pub data: Option<Buffer>,
}

impl VfsRequest {
    fn path(path: &str) -> Self {
        Self {
            path: Some(path.to_owned()),
            ..Self::default()
        }
    }
}

#[napi(object)]
pub struct VfsResponse {
    pub file_type: Option<String>,
    pub len: Option<f64>,
    pub entries: Option<Vec<DirEntryJs>>,
    pub handle: Option<f64>,
    pub bytes_read: Option<f64>,
    pub bytes_written: Option<f64>,
    pub data: Option<Buffer>,
    pub used_bytes: Option<f64>,
    pub file_count: Option<f64>,
    pub error: Option<VfsCallbackError>,
}

#[napi(object)]
pub struct VfsCallbackError {
    pub code: Option<String>,
    pub message: Option<String>,
}

#[napi(object)]
#[derive(Clone, Copy, Default)]
pub struct OpenModeJs {
    pub read: Option<bool>,
    pub write: Option<bool>,
    pub create: Option<bool>,
    pub create_new: Option<bool>,
    pub truncate: Option<bool>,
    pub append: Option<bool>,
}

impl From<OpenModeJs> for OpenMode {
    fn from(mode: OpenModeJs) -> Self {
        Self {
            read: mode.read.unwrap_or(false),
            write: mode.write.unwrap_or(false),
            create: mode.create.unwrap_or(false),
            create_new: mode.create_new.unwrap_or(false),
            truncate: mode.truncate.unwrap_or(false),
            append: mode.append.unwrap_or(false),
        }
    }
}

impl From<OpenMode> for OpenModeJs {
    fn from(mode: OpenMode) -> Self {
        Self {
            read: Some(mode.read),
            write: Some(mode.write),
            create: Some(mode.create),
            create_new: Some(mode.create_new),
            truncate: Some(mode.truncate),
            append: Some(mode.append),
        }
    }
}

#[napi(object)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub wall_time_ms: f64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub pipe_bytes: Vec<f64>,
    pub commands: Vec<CommandTiming>,
    pub peak_wasm_memory_bytes: Option<f64>,
}

impl From<CoreExecResult> for ExecResult {
    fn from(result: CoreExecResult) -> Self {
        Self {
            stdout: result.stdout,
            stderr: result.stderr,
            exit_code: result.exit_code,
            wall_time_ms: result.metrics.wall_time.as_secs_f64() * 1000.0,
            stdout_truncated: result.metrics.stdout_truncated,
            stderr_truncated: result.metrics.stderr_truncated,
            pipe_bytes: result
                .metrics
                .pipe_bytes
                .into_iter()
                .map(|bytes| bytes as f64)
                .collect(),
            commands: result
                .metrics
                .commands
                .into_iter()
                .map(CommandTiming::from)
                .collect(),
            peak_wasm_memory_bytes: result
                .metrics
                .peak_wasm_memory_bytes
                .map(|bytes| bytes as f64),
        }
    }
}

#[napi(object)]
pub struct CommandTiming {
    pub name: String,
    pub duration_ms: f64,
    pub exit_code: i32,
}

impl From<tinysandbox::sandbox::CommandTiming> for CommandTiming {
    fn from(timing: tinysandbox::sandbox::CommandTiming) -> Self {
        Self {
            name: timing.name,
            duration_ms: timing.duration.as_secs_f64() * 1000.0,
            exit_code: timing.exit_code,
        }
    }
}

#[napi(object)]
pub struct FileStat {
    pub file_type: String,
    pub len: f64,
    pub is_file: bool,
    pub is_dir: bool,
}

impl From<Metadata> for FileStat {
    fn from(metadata: Metadata) -> Self {
        Self {
            file_type: file_type_name(metadata.file_type).to_owned(),
            len: metadata.len as f64,
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
        }
    }
}

#[napi(object)]
pub struct DirEntryJs {
    pub name: String,
    pub file_type: String,
    pub len: f64,
}

impl From<DirEntry> for DirEntryJs {
    fn from(entry: DirEntry) -> Self {
        Self {
            name: entry.name,
            file_type: file_type_name(entry.metadata.file_type).to_owned(),
            len: entry.metadata.len as f64,
        }
    }
}

impl TryFrom<DirEntryJs> for DirEntry {
    type Error = VfsError;

    fn try_from(entry: DirEntryJs) -> VfsResult<Self> {
        Ok(Self {
            name: entry.name,
            metadata: Metadata {
                file_type: parse_file_type(&entry.file_type)?,
                len: u64_from_js(entry.len)?,
            },
        })
    }
}

#[napi(object)]
pub struct SandboxStats {
    pub commands_run: f64,
    pub vfs: Option<VfsStatsJs>,
}

#[napi(object)]
pub struct ConformanceResult {
    pub ok: bool,
    pub snapshots: String,
}

#[napi(object)]
pub struct VfsQuotaJs {
    pub max_bytes: f64,
    pub max_files: f64,
    pub max_file_size: f64,
}

impl From<VfsQuota> for VfsQuotaJs {
    fn from(quota: VfsQuota) -> Self {
        Self {
            max_bytes: quota.max_bytes as f64,
            max_files: quota.max_files as f64,
            max_file_size: quota.max_file_size as f64,
        }
    }
}

#[napi(object)]
pub struct VfsStatsJs {
    pub used_bytes: f64,
    pub file_count: f64,
}

impl From<VfsStats> for VfsStatsJs {
    fn from(stats: VfsStats) -> Self {
        Self {
            used_bytes: stats.used_bytes as f64,
            file_count: stats.file_count as f64,
        }
    }
}

fn parse_limits(limits: Object<'_>) -> Result<Limits> {
    let mut parsed = Limits::default();
    if let Some(ms) = get_optional::<f64>(&limits, "wallTimeMs")? {
        if !ms.is_finite() || ms < 0.0 {
            return Err(Error::new(
                Status::InvalidArg,
                "wallTimeMs must be a finite non-negative number".to_owned(),
            ));
        }
        parsed.wall_time = Duration::from_secs_f64(ms / 1000.0);
    }
    if let Some(bytes) = get_optional::<f64>(&limits, "stdoutBytes")? {
        parsed.stdout_bytes = usize_from_number(bytes)?;
    }
    if let Some(bytes) = get_optional::<f64>(&limits, "stderrBytes")? {
        parsed.stderr_bytes = usize_from_number(bytes)?;
    }
    if let Some(commands) = get_optional::<f64>(&limits, "maxCommands")? {
        parsed.max_commands = usize_from_number(commands)?;
    }
    if let Some(bytes) = get_optional::<f64>(&limits, "sortInputBytes")? {
        parsed.sort_input_bytes = usize_from_number(bytes)?;
    }
    if let Some(bytes) = get_optional::<f64>(&limits, "jqInputBytes")? {
        parsed.jq_input_bytes = usize_from_number(bytes)?;
    }
    if let Some(bytes) = get_optional::<f64>(&limits, "wasmMemoryBytes")? {
        parsed.wasm_memory_bytes = usize_from_number(bytes)?;
    }
    if let Some(bytes) = get_optional::<f64>(&limits, "fetchResponseBytes")? {
        parsed.fetch_response_bytes = usize_from_number(bytes)?;
    }
    Ok(parsed)
}

fn validate_syscall_name(name: &str) -> Result<()> {
    if !is_js_syscall_name(name) {
        return Err(Error::new(
            Status::InvalidArg,
            format!(
                "Sandbox constructor cannot register invalid syscall name '{name}'; names must match [A-Za-z_][A-Za-z0-9_]*"
            ),
        ));
    }
    if name == "fetch" {
        return Err(Error::new(
            Status::InvalidArg,
            "Sandbox constructor cannot register reserved syscall name 'fetch'; use the fetch option"
                .to_owned(),
        ));
    }
    Ok(())
}

fn is_js_syscall_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn get_optional<T>(object: &Object<'_>, name: &str) -> Result<Option<T>>
where
    T: napi::bindgen_prelude::FromNapiValue + napi::bindgen_prelude::ValidateNapiValue,
{
    if object.has_named_property(name)? {
        object.get_named_property(name).map(Some)
    } else {
        Ok(None)
    }
}

fn get_optional_object<'env>(object: &Object<'env>, name: &str) -> Result<Option<Object<'env>>> {
    if object.has_named_property(name)? {
        object.get_named_property(name).map(Some)
    } else {
        Ok(None)
    }
}

fn vfs_callback(vfs: &Object<'_>, name: &'static str) -> Result<JsVfsCallback> {
    let callback: Function<'_, (VfsRequest,), Promise<VfsResponse>> =
        vfs.get_named_property(name)?;
    Ok(Arc::new(
        callback
            .build_threadsafe_function::<VfsRequest>()
            .callee_handled::<false>()
            .weak::<true>()
            .build_callback(|ctx| Ok((ctx.value,)))?,
    ))
}

fn response_metadata(response: VfsResponse) -> VfsResult<Metadata> {
    Ok(Metadata {
        file_type: parse_file_type(
            response
                .file_type
                .as_deref()
                .ok_or(VfsError::new(Errno::EINVAL))?,
        )?,
        len: u64_from_js(response.len.ok_or(VfsError::new(Errno::EINVAL))?)?,
    })
}

fn response_stats(response: VfsResponse) -> VfsResult<VfsStats> {
    Ok(VfsStats {
        used_bytes: u64_from_js(response.used_bytes.ok_or(VfsError::new(Errno::EINVAL))?)?,
        file_count: u64_from_js(response.file_count.ok_or(VfsError::new(Errno::EINVAL))?)?,
    })
}

fn napi_vfs_error(err: VfsError, path: Option<&str>) -> Error {
    let code = err.errno().name();
    let message = match path {
        Some(path) => format!("{code}: {path}"),
        None => code.to_owned(),
    };
    Error::new(Status::GenericFailure, message)
}

fn errno_from_code(code: Option<&str>) -> Errno {
    match code {
        Some("EBADF") => Errno::EBADF,
        Some("EBUSY") => Errno::EBUSY,
        Some("EACCES") => Errno::EACCES,
        Some("EEXIST") => Errno::EEXIST,
        Some("EINVAL") => Errno::EINVAL,
        Some("EISDIR") => Errno::EISDIR,
        Some("ENOENT") => Errno::ENOENT,
        Some("ENOSPC") => Errno::ENOSPC,
        Some("ENOTDIR") => Errno::ENOTDIR,
        Some("ENOTEMPTY") => Errno::ENOTEMPTY,
        _ => Errno::EINVAL,
    }
}

fn file_type_name(file_type: FileType) -> &'static str {
    match file_type {
        FileType::File => "file",
        FileType::Directory => "directory",
    }
}

fn parse_file_type(file_type: &str) -> VfsResult<FileType> {
    match file_type {
        "file" => Ok(FileType::File),
        "directory" => Ok(FileType::Directory),
        _ => Err(VfsError::new(Errno::EINVAL)),
    }
}

fn handle_from_js(value: f64) -> VfsResult<FileHandle> {
    u64_from_js(value).map(FileHandle::new)
}

fn u64_from_js(value: f64) -> VfsResult<u64> {
    if value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= MAX_SAFE_INTEGER {
        Ok(value as u64)
    } else {
        Err(VfsError::new(Errno::EINVAL))
    }
}

fn usize_from_js(value: f64) -> VfsResult<usize> {
    f64_to_usize_lossless(value)
}

fn f64_to_usize_lossless(value: f64) -> VfsResult<usize> {
    if value.is_finite()
        && value >= 0.0
        && value.fract() == 0.0
        && value <= MAX_SAFE_INTEGER
        && value <= usize::MAX as f64
    {
        Ok(value as usize)
    } else {
        Err(VfsError::new(Errno::EINVAL))
    }
}

fn usize_from_number(value: f64) -> Result<usize> {
    f64_to_usize_lossless(value)
        .map_err(|err| Error::new(Status::InvalidArg, err.errno().name().to_owned()))
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "conformance suite panicked".to_owned()
    }
}
