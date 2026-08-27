use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use napi::bindgen_prelude::{
    AsyncTask, Buffer, External, FromNapiValue, Function, JsObjectValue, Object, Promise, Unknown,
    ValueType,
};
use napi::threadsafe_function::ThreadsafeFunction;
use napi::{Error, JsExternal, Result, Status, Task};
use napi_derive::napi;
use serde_json::Value;
use tinysandbox::sandbox::{
    Command, CommandContext, CommandFuture, CommandResult, ExecResult as CoreExecResult,
    FetchRequest as CoreFetchRequest, FetchResponse as CoreFetchResponse, HostError, Limits,
    Sandbox as CoreSandbox,
};
use tinysandbox::vfs::{
    DirEntry, Errno, FileHandle, FileType, InMemoryVfs, Metadata, OpenMode, Vfs, VfsError,
    VfsQuota, VfsResult, VfsStats,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type JsCommandCallback = Arc<
    ThreadsafeFunction<CommandCall, Promise<CommandOutput>, (CommandCall,), Status, false, true>,
>;
type JsGlobalCallback = Arc<
    ThreadsafeFunction<Value, Promise<JsGlobalCallbackResponse>, (Value,), Status, false, true>,
>;
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

#[napi]
pub const PROMPT_OVERVIEW: &str = tinysandbox::prompts::OVERVIEW;
#[napi]
pub const PROMPT_SHELL: &str = tinysandbox::prompts::SHELL;
#[napi]
pub const PROMPT_BUILTINS: &str = tinysandbox::prompts::BUILTINS;
#[napi]
pub const PROMPT_JQ: &str = tinysandbox::prompts::JQ;
#[napi]
pub const PROMPT_JS: &str = tinysandbox::prompts::JS;
#[napi]
pub fn prompt_globals(names: Vec<String>) -> String {
    tinysandbox::prompts::globals(names)
}

/// Compiles the embedded QuickJS module and returns the machine-code artifact.
#[napi]
pub fn precompile_js() -> Result<Buffer> {
    tinysandbox::js::precompile()
        .map(Buffer::from)
        .map_err(|err| Error::new(Status::GenericFailure, err.to_string()))
}

/// Installs a `precompileJs` artifact as this process's JavaScript runtime.
#[napi]
pub fn use_precompiled_js(artifact: Buffer) -> Result<()> {
    tinysandbox::js::use_precompiled(&artifact)
        .map_err(|err| Error::new(Status::GenericFailure, err.to_string()))
}

/// Reports where this process's JavaScript machine code came from.
#[napi]
pub fn js_runtime_source() -> Result<String> {
    tinysandbox::js::runtime_source()
        .map(|source| match source {
            tinysandbox::js::RuntimeSource::Precompiled => "precompiled".to_owned(),
            tinysandbox::js::RuntimeSource::Compiled => "compiled".to_owned(),
        })
        .map_err(|err| Error::new(Status::GenericFailure, err.to_string()))
}
#[napi]
pub const PROMPT_FETCH: &str = tinysandbox::prompts::FETCH;
#[napi]
pub const PROMPT_SESSION_EPHEMERAL: &str = tinysandbox::prompts::SESSION_EPHEMERAL;
#[napi]
pub const PROMPT_SESSION_PERSISTENT: &str = tinysandbox::prompts::SESSION_PERSISTENT;

#[napi(js_name = "NativeSandbox")]
pub struct Sandbox {
    // Napi owns this Arc through the JS object finalizer, so the final drop of
    // JsVfs runtimes stays outside Tokio async contexts.
    inner: Arc<CoreSandbox>,
    // Concrete handle kept alongside the type-erased one in the sandbox so
    // refreshLocalVfs/setLocalVfsUsage can reach the LocalVfs-only API.
    #[cfg(unix)]
    local_vfs: HashMap<String, Arc<tinysandbox::vfs::LocalVfs>>,
}

#[napi]
impl Sandbox {
    #[napi(constructor)]
    pub fn new(options: Option<Object<'_>>) -> Result<Self> {
        let mut builder = CoreSandbox::builder();
        #[cfg(unix)]
        let mut local_vfs = HashMap::new();

        if let Some(options) = options {
            for removed in ["vfs", "localVfs", "s3Vfs"] {
                if has_non_nullish_named_property(&options, removed)? {
                    return Err(Error::new(
                        Status::InvalidArg,
                        format!("Sandbox option '{removed}' was replaced by the mounts option"),
                    ));
                }
            }
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
            if let Some(globals) = get_optional_object(&options, "globals")? {
                for name in Object::keys(&globals)? {
                    validate_js_global_name(&name)?;
                    let callback: Function<'_, (Value,), Promise<JsGlobalCallbackResponse>> =
                        globals.get_named_property(&name)?;
                    let callback = Arc::new(
                        callback
                            .build_threadsafe_function::<Value>()
                            .callee_handled::<false>()
                            .weak::<true>()
                            .build_callback(|ctx| Ok((ctx.value,)))?,
                    );
                    builder = builder.js_global(name, move |args| {
                        let callback = Arc::clone(&callback);
                        async move { call_js_global(callback, args).await }
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
            if let Some(mounts) = get_optional_object(&options, "mounts")? {
                builder = builder.clear_mounts();
                for name in Object::keys(&mounts)? {
                    tinysandbox::vfs::mount::validate_mount_name(&name).map_err(|_| {
                        Error::new(
                            Status::InvalidArg,
                            format!(
                                "mount name '{name}' must be a non-reserved single path component"
                            ),
                        )
                    })?;
                    let mount: Object<'_> = mounts.get_named_property(&name)?;
                    let kind: String = mount.get_named_property("type")?;
                    let vfs: Arc<dyn Vfs> = match kind.as_str() {
                        "memory" => Arc::new(InMemoryVfs::new(parse_vfs_quota(
                            &mount,
                            &format!("memory mount '{name}'"),
                        )?)),
                        "custom" => {
                            let custom: Object<'_> = mount.get_named_property("vfs")?;
                            Arc::new(JsVfs::new(custom)?)
                        }
                        #[cfg(unix)]
                        "local" => {
                            let local = build_local_vfs(&mount, &name)?;
                            local_vfs.insert(name.clone(), Arc::clone(&local));
                            local
                        }
                        #[cfg(not(unix))]
                        "local" => {
                            return Err(Error::new(
                                Status::InvalidArg,
                                format!("local mount '{name}' is only supported on Unix hosts"),
                            ));
                        }
                        "s3" => build_s3_vfs(&mount)?,
                        _ => {
                            return Err(Error::new(
                                Status::InvalidArg,
                                format!("mount '{name}' type must be memory, local, s3, or custom"),
                            ));
                        }
                    };
                    builder = builder.mount_arc(name, vfs);
                }
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
            #[cfg(unix)]
            local_vfs,
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

    /// Rescans a local mount's host directory and replaces its quota usage.
    /// Call this after the host mutates the directory outside the sandbox.
    #[napi]
    pub async fn refresh_local_vfs(&self, mount: String) -> Result<VfsStatsJs> {
        #[cfg(unix)]
        {
            let vfs = self
                .local_vfs
                .get(&mount)
                .cloned()
                .ok_or_else(|| local_vfs_missing(&mount))?;
            tokio::task::spawn_blocking(move || {
                vfs.refresh()
                    .map(VfsStatsJs::from)
                    .map_err(|err| napi_vfs_error(err, None))
            })
            .await
            .map_err(|err| Error::new(Status::GenericFailure, err.to_string()))?
        }
        #[cfg(not(unix))]
        {
            Err(local_vfs_missing(&mount))
        }
    }

    /// Replaces a local mount's quota usage with externally computed numbers.
    /// Later file operations apply their deltas on top of the pushed baseline.
    #[napi]
    pub fn set_local_vfs_usage(&self, mount: String, usage: VfsStatsJs) -> Result<()> {
        #[cfg(unix)]
        {
            let vfs = self
                .local_vfs
                .get(&mount)
                .ok_or_else(|| local_vfs_missing(&mount))?;
            let invalid = |field: &str| {
                Error::new(
                    Status::InvalidArg,
                    format!(
                        "local mount '{mount}' usage {field} must be a non-negative safe integer"
                    ),
                )
            };
            vfs.set_usage(VfsStats {
                used_bytes: u64_from_js(usage.used_bytes).map_err(|_| invalid("usedBytes"))?,
                file_count: u64_from_js(usage.file_count).map_err(|_| invalid("fileCount"))?,
            });
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = usage;
            Err(local_vfs_missing(&mount))
        }
    }
}

fn local_vfs_missing(mount: &str) -> Error {
    Error::new(
        Status::GenericFailure,
        format!("sandbox mount '{mount}' is not a local filesystem"),
    )
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

async fn call_js_global(
    callback: JsGlobalCallback,
    args: Value,
) -> std::result::Result<Value, HostError> {
    let promise = callback
        .call_async_catch(args)
        .await
        .map_err(|err| HostError::new(err.reason))?;
    let response = promise.await.map_err(|err| HostError::new(err.reason))?;
    if let Some(error) = response.error {
        return Err(host_error_from_callback(error));
    }
    Ok(response.value.unwrap_or(Value::Null))
}

async fn call_js_fetch(
    callback: JsFetchCallback,
    request: CoreFetchRequest,
) -> std::result::Result<CoreFetchResponse, HostError> {
    let promise = callback
        .call_async_catch(FetchRequest::from(request))
        .await
        .map_err(|err| HostError::new(err.reason))?;
    let response = promise.await.map_err(|err| HostError::new(err.reason))?;
    if let Some(error) = response.error {
        return Err(host_error_from_callback(error));
    }
    let response = response
        .response
        .ok_or_else(|| HostError::new("fetch handler did not return a response"))?;
    Ok(CoreFetchResponse {
        status: status_from_js(response.status)?,
        headers: header_pairs_from_js(response.headers.unwrap_or_default())?,
        body: response.body.map(|body| body.to_vec()).unwrap_or_default(),
    })
}

fn host_error_from_callback(error: HostCallbackError) -> HostError {
    let message = error
        .message
        .unwrap_or_else(|| "host callback failed".to_owned());
    match error.code {
        Some(code) => HostError::new(message).with_code(code),
        None => HostError::new(message),
    }
}

fn status_from_js(status: Option<f64>) -> std::result::Result<u16, HostError> {
    let status = status.ok_or_else(|| HostError::new("fetch response status is required"))?;
    if status.is_finite() && status.fract() == 0.0 && (100.0..=599.0).contains(&status) {
        Ok(status as u16)
    } else {
        Err(HostError::new(
            "fetch response status must be an integer from 100 through 599",
        ))
    }
}

fn header_pairs_from_js(
    headers: Vec<Vec<String>>,
) -> std::result::Result<Vec<(String, String)>, HostError> {
    headers
        .into_iter()
        .map(|pair| match pair.as_slice() {
            [name, value] => Ok((name.clone(), value.clone())),
            _ => Err(HostError::new(
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
pub struct JsGlobalCallbackResponse {
    pub value: Option<Value>,
    pub error: Option<HostCallbackError>,
}

#[napi(object)]
pub struct HostCallbackError {
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
    pub error: Option<HostCallbackError>,
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

struct S3ClientOptions {
    region: Option<String>,
    endpoint_url: Option<String>,
    force_path_style: Option<bool>,
    credentials: Option<S3Credentials>,
}

struct S3Credentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

fn build_s3_vfs(options: &Object<'_>) -> Result<Arc<tinysandbox::vfs::S3Vfs>> {
    let bucket = required_nonempty_string(options, "bucket", "S3 mount")?;
    let prefix = get_optional::<String>(options, "prefix")?;
    let region = optional_nonempty_string(options, "region", "S3 mount")?;
    let endpoint_url = optional_nonempty_string(options, "endpointUrl", "S3 mount")?;
    let force_path_style = get_optional::<bool>(options, "forcePathStyle")?;
    let credentials = get_optional_object(options, "credentials")?
        .map(|credentials| parse_s3_credentials(&credentials))
        .transpose()?;
    let config = parse_s3_config(options)?;

    let client = build_s3_client(S3ClientOptions {
        region,
        endpoint_url,
        force_path_style,
        credentials,
    })?;
    let vfs = tinysandbox::vfs::S3Vfs::with_config(client, bucket, prefix.as_deref(), config)
        .map_err(|err| {
            Error::new(
                Status::InvalidArg,
                format!("invalid S3 mount bucket or prefix: {err}"),
            )
        })?;
    Ok(Arc::new(vfs))
}

fn parse_s3_config(options: &Object<'_>) -> Result<tinysandbox::vfs::S3VfsConfig> {
    let mut config = tinysandbox::vfs::S3VfsConfig::default();
    if let Some(read_only) = get_optional::<bool>(options, "readOnly")? {
        config.read_only = read_only;
    }
    if let Some(directory_rename) = get_optional::<bool>(options, "directoryRename")? {
        config.directory_rename = directory_rename;
    }
    if let Some(conditional_writes) = get_optional::<bool>(options, "conditionalWrites")? {
        config.conditional_writes = conditional_writes;
    }
    if let Some(max_edit_bytes) = get_optional::<f64>(options, "maxEditBytes")? {
        config.max_edit_bytes = u64_from_js(max_edit_bytes).map_err(|_| {
            Error::new(
                Status::InvalidArg,
                "S3 mount maxEditBytes must be a non-negative safe integer".to_owned(),
            )
        })?;
    }
    Ok(config)
}

fn parse_s3_credentials(credentials: &Object<'_>) -> Result<S3Credentials> {
    Ok(S3Credentials {
        access_key_id: required_nonempty_string(
            credentials,
            "accessKeyId",
            "S3 mount credentials",
        )?,
        secret_access_key: required_nonempty_string(
            credentials,
            "secretAccessKey",
            "S3 mount credentials",
        )?,
        session_token: optional_nonempty_string(
            credentials,
            "sessionToken",
            "S3 mount credentials",
        )?,
    })
}

fn required_nonempty_string(object: &Object<'_>, name: &str, scope: &str) -> Result<String> {
    if !has_non_nullish_named_property(object, name)? {
        return Err(Error::new(
            Status::InvalidArg,
            format!("{scope} {name} is required and must be a nonempty string"),
        ));
    }
    let value: String = object.get_named_property(name)?;
    validate_nonempty_string(value, name, scope)
}

fn optional_nonempty_string(
    object: &Object<'_>,
    name: &str,
    scope: &str,
) -> Result<Option<String>> {
    get_optional::<String>(object, name)?
        .map(|value| validate_nonempty_string(value, name, scope))
        .transpose()
}

fn validate_nonempty_string(value: String, name: &str, scope: &str) -> Result<String> {
    if value.trim().is_empty() {
        return Err(Error::new(
            Status::InvalidArg,
            format!("{scope} {name} must be a nonempty string"),
        ));
    }
    Ok(value)
}

fn build_s3_client(options: S3ClientOptions) -> Result<aws_sdk_s3::Client> {
    let thread = std::thread::Builder::new()
        .name("tinysandbox-s3-config".to_owned())
        .spawn(move || -> std::result::Result<aws_sdk_s3::Client, String> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| format!("could not create configuration runtime: {err}"))?;
            runtime.block_on(async move {
                let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
                if let Some(region) = options.region {
                    loader = loader.region(aws_sdk_s3::config::Region::new(region));
                }
                if let Some(credentials) = options.credentials {
                    loader = loader.credentials_provider(aws_sdk_s3::config::Credentials::new(
                        credentials.access_key_id,
                        credentials.secret_access_key,
                        credentials.session_token,
                        None,
                        "tinysandbox-node-s3-mount",
                    ));
                }
                let shared_config = loader.load().await;
                let mut service_config = aws_sdk_s3::config::Builder::from(&shared_config);
                if let Some(endpoint_url) = options.endpoint_url {
                    service_config = service_config.endpoint_url(endpoint_url);
                }
                if let Some(force_path_style) = options.force_path_style {
                    service_config = service_config.force_path_style(force_path_style);
                }
                Ok(aws_sdk_s3::Client::from_conf(service_config.build()))
            })
        })
        .map_err(|err| {
            Error::new(
                Status::GenericFailure,
                format!("could not start S3 mount configuration: {err}"),
            )
        })?;

    match thread.join() {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(message)) => Err(Error::new(
            Status::GenericFailure,
            format!("could not configure S3 mount: {message}"),
        )),
        Err(_) => Err(Error::new(
            Status::GenericFailure,
            "could not configure S3 mount: configuration worker panicked".to_owned(),
        )),
    }
}

#[cfg(unix)]
fn build_local_vfs(options: &Object<'_>, mount: &str) -> Result<Arc<tinysandbox::vfs::LocalVfs>> {
    let root: String = options.get_named_property("root")?;
    let quota = parse_vfs_quota(options, &format!("local mount '{mount}'"))?;

    let vfs = tinysandbox::vfs::LocalVfs::with_quota(&root, quota).map_err(|err| {
        Error::new(
            Status::InvalidArg,
            format!("local mount '{mount}' root '{root}': {err}"),
        )
    })?;
    Ok(Arc::new(vfs))
}

fn parse_vfs_quota(options: &Object<'_>, context: &str) -> Result<VfsQuota> {
    let mut quota = VfsQuota::unlimited();
    if let Some(limits) = get_optional_object(options, "quota")? {
        if let Some(value) = get_optional::<f64>(&limits, "maxBytes")? {
            quota.max_bytes = quota_value(value, context, "maxBytes")?;
        }
        if let Some(value) = get_optional::<f64>(&limits, "maxFiles")? {
            quota.max_files = quota_value(value, context, "maxFiles")?;
        }
        if let Some(value) = get_optional::<f64>(&limits, "maxFileSize")? {
            quota.max_file_size = quota_value(value, context, "maxFileSize")?;
        }
    }
    Ok(quota)
}

fn quota_value(value: f64, context: &str, name: &str) -> Result<u64> {
    u64_from_js(value).map_err(|_| {
        Error::new(
            Status::InvalidArg,
            format!("{context} quota {name} must be a non-negative safe integer"),
        )
    })
}

fn validate_js_global_name(name: &str) -> Result<()> {
    if !is_js_global_name(name) {
        return Err(Error::new(
            Status::InvalidArg,
            format!(
                "Sandbox constructor cannot register invalid global name '{name}'; names are dot-separated paths of [A-Za-z_][A-Za-z0-9_]* segments"
            ),
        ));
    }
    let root = name.split('.').next().unwrap_or(name);
    // Mirrors RESERVED_JS_GLOBALS in the core crate.
    const RESERVED: &[&str] = &[
        "Buffer",
        "Headers",
        "Response",
        "__dirname",
        "__filename",
        "console",
        "exports",
        "fetch",
        "globalThis",
        "module",
        "process",
        "require",
    ];
    if RESERVED.contains(&root) {
        return Err(Error::new(
            Status::InvalidArg,
            format!(
                "Sandbox constructor cannot register reserved global name '{name}'; '{root}' is provided by the JavaScript runtime"
            ),
        ));
    }
    Ok(())
}

fn is_js_global_name(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(is_js_global_segment)
}

fn is_js_global_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn get_optional<T>(object: &Object<'_>, name: &str) -> Result<Option<T>>
where
    T: napi::bindgen_prelude::FromNapiValue + napi::bindgen_prelude::ValidateNapiValue,
{
    if has_non_nullish_named_property(object, name)? {
        object.get_named_property(name).map(Some)
    } else {
        Ok(None)
    }
}

fn get_optional_object<'env>(object: &Object<'env>, name: &str) -> Result<Option<Object<'env>>> {
    if has_non_nullish_named_property(object, name)? {
        object.get_named_property(name).map(Some)
    } else {
        Ok(None)
    }
}

fn has_non_nullish_named_property(object: &Object<'_>, name: &str) -> Result<bool> {
    if !object.has_named_property(name)? {
        return Ok(false);
    }
    let value: Unknown<'_> = object.get_named_property(name)?;
    Ok(!matches!(
        value.get_type()?,
        ValueType::Null | ValueType::Undefined
    ))
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
        Some("EXDEV") => Errno::EXDEV,
        Some("EACCES") => Errno::EACCES,
        Some("EEXIST") => Errno::EEXIST,
        Some("EFBIG") => Errno::EFBIG,
        Some("EIO") => Errno::EIO,
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
