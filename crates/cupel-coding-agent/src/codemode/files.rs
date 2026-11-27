use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use serde::{Deserialize, Serialize};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::codemode::coordinator::{cleanup_until, stop_and_reap};
use crate::codemode::{CALLBACK_BYTES, Failure, frame};
use crate::read_filesystem::{LocalFiles, PageRevision, ReadFilesystem, SnapshotKind};
use crate::search::GrepSearch;
use crate::tools::grep::{self, GrepArgs};
use crate::tools::read::ReadArgs;

pub const FILE_WORKER_ARGUMENT: &str = "--internal-codemode-file-worker";
const REQUEST_BYTES: usize = 32 * 1024;
const SNAPSHOT_REPLY_BYTES: usize = 6 * 1024 * 1024 + 4096;
const FILE_WORKER_BYTES: usize = 128 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
enum FileRequest {
    Canonicalize(OsString),
    Snapshot {
        path: OsString,
        kind: SnapshotKind,
    },
    Page {
        path: OsString,
        args: ReadArgs,
    },
    Grep {
        cwd: OsString,
        root: OsString,
        args: GrepArgs,
    },
}

impl FileRequest {
    fn reply_limit(&self) -> usize {
        match self {
            Self::Snapshot { .. } => SNAPSHOT_REPLY_BYTES,
            Self::Canonicalize(_) => REQUEST_BYTES,
            Self::Page { .. } | Self::Grep { .. } => CALLBACK_BYTES + 4096,
        }
    }
}

pub(super) struct FileWorker {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
}

impl FileWorker {
    pub(super) fn spawn(binary: &Path, cwd: &Path) -> Result<Self, Failure> {
        let mut command = Command::new(binary);
        command
            .arg(FILE_WORKER_ARGUMENT)
            .current_dir(cwd)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for name in [
            "HOME",
            "XDG_CONFIG_HOME",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().map_err(|_error| Failure::Worker)?;
        match (child.stdin.take(), child.stdout.take()) {
            (Some(input), Some(output)) => Ok(Self {
                child,
                input,
                output,
            }),
            _ => Err(Failure::Cleanup),
        }
    }

    pub(super) async fn stop(mut self, return_by: Instant) -> Result<(), Failure> {
        stop_and_reap(&mut self.child, cleanup_until(return_by)).await
    }

    pub(super) async fn grep(
        &mut self,
        cwd: &Path,
        root: &Path,
        args: GrepArgs,
    ) -> Result<serde_json::Value, Failure> {
        self.request(FileRequest::Grep {
            cwd: cwd.as_os_str().to_owned(),
            root: root.as_os_str().to_owned(),
            args,
        })
        .await
        .map_err(|error| {
            if error.kind() == io::ErrorKind::FileTooLarge {
                Failure::CallbackBytes
            } else {
                Failure::Callback
            }
        })
    }

    async fn request<T>(&mut self, request: FileRequest) -> io::Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut buffer = frame::Buffer::new(REQUEST_BYTES);
        serde_json::to_writer(&mut buffer, &request)?;
        frame::write_async(&mut self.input, frame::FILE_REQUEST, &buffer.into_bytes())
            .await
            .map_err(io::Error::other)?;
        let (_, bytes) = frame::read_async(&mut self.output, |tag| match tag {
            frame::FILE_REPLY => Ok(request.reply_limit()),
            _ => Err(Failure::Worker),
        })
        .await
        .map_err(io::Error::other)?;
        let result: Result<T, FileError> = serde_json::from_slice(&bytes)?;
        result.map_err(|error| match error {
            FileError::TooLarge => io::Error::new(
                io::ErrorKind::FileTooLarge,
                "file worker result exceeds the IPC limit",
            ),
            FileError::Operation => io::Error::other("file worker operation failed"),
        })
    }
}

#[async_trait::async_trait]
impl ReadFilesystem for FileWorker {
    async fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf> {
        let path: OsString = self
            .request(FileRequest::Canonicalize(path.as_os_str().to_owned()))
            .await?;
        Ok(path.into())
    }

    async fn snapshot(&mut self, path: &Path, kind: SnapshotKind) -> io::Result<String> {
        self.request(FileRequest::Snapshot {
            path: path.as_os_str().to_owned(),
            kind,
        })
        .await
    }

    async fn page(&mut self, path: &Path, args: &ReadArgs) -> io::Result<PageRevision> {
        self.request(FileRequest::Page {
            path: path.as_os_str().to_owned(),
            args: args.clone(),
        })
        .await
    }
}

#[derive(Serialize, Deserialize)]
enum FileError {
    TooLarge,
    Operation,
}

pub fn file_worker_main() -> ExitCode {
    if monty_alloc::set_hard_limit(Some(FILE_WORKER_BYTES)).is_err() {
        return ExitCode::FAILURE;
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return ExitCode::FAILURE;
    };
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    loop {
        let Ok((_, bytes)) = frame::read(&mut input, |tag| match tag {
            frame::FILE_REQUEST => Ok(REQUEST_BYTES),
            _ => Err(Failure::Worker),
        }) else {
            return ExitCode::FAILURE;
        };
        let Ok(request) = serde_json::from_slice(&bytes) else {
            return ExitCode::FAILURE;
        };
        if runtime.block_on(serve(request, &mut output)).is_err() {
            return ExitCode::FAILURE;
        }
    }
}

async fn serve(request: FileRequest, output: &mut impl io::Write) -> Result<(), Failure> {
    let limit = request.reply_limit();
    match request {
        FileRequest::Canonicalize(path) => reply(
            output,
            limit,
            LocalFiles
                .canonicalize(Path::new(&path))
                .await
                .map(PathBuf::into_os_string),
        ),
        FileRequest::Snapshot { path, kind } => reply(
            output,
            limit,
            LocalFiles.snapshot(Path::new(&path), kind).await,
        ),
        FileRequest::Page { path, args } => reply(
            output,
            limit,
            LocalFiles.page(Path::new(&path), &args).await,
        ),
        FileRequest::Grep { cwd, root, args } => {
            let backend = GrepSearch::new(root);
            let result = grep::collect(Path::new(&cwd), &backend, args, CancellationToken::new())
                .await
                .and_then(|report| report.into_callback().map_err(Into::into));
            reply(output, limit, result)
        }
    }
}

fn reply<T, E>(
    output: &mut impl io::Write,
    limit: usize,
    result: Result<T, E>,
) -> Result<(), Failure>
where
    T: Serialize,
{
    let mut buffer = frame::Buffer::new(limit);
    if serde_json::to_writer(&mut buffer, &result.map_err(|_error| FileError::Operation)).is_err() {
        buffer = frame::Buffer::new(limit);
        serde_json::to_writer(&mut buffer, &Err::<(), _>(FileError::TooLarge))
            .map_err(|_error| Failure::Worker)?;
    }
    frame::write(output, frame::FILE_REPLY, &buffer.into_bytes())
}
