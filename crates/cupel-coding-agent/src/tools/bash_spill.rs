use std::io::{self, Write as _};
use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::jobs::Jobs;

pub(crate) struct SpillChunk {
    sender: mpsc::Sender<Vec<u8>>,
    bytes: Vec<u8>,
}

impl SpillChunk {
    pub(crate) async fn send(&mut self) {
        if let Ok(permit) = self.sender.reserve().await {
            permit.send(std::mem::take(&mut self.bytes));
        }
    }
}

pub(super) struct Spill {
    sender: mpsc::Sender<Vec<u8>>,
    worker: JoinHandle<io::Result<SpillLog>>,
}

impl Spill {
    pub(super) fn start() -> Option<Self> {
        let nonce = getrandom::u64().ok()?;
        let path = std::env::temp_dir().join(format!(
            "cupel-bash-{}-{nonce:016x}.log",
            std::process::id()
        ));
        let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(4);
        let worker = tokio::task::spawn_blocking(move || {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let file = options.open(&path)?;
            let log = SpillLog { path: Some(path) };
            let mut writer = io::BufWriter::new(file);
            while let Some(bytes) = receiver.blocking_recv() {
                writer.write_all(&bytes)?;
            }
            writer.flush()?;
            Ok(log)
        });
        Some(Self { sender, worker })
    }

    pub(super) fn chunk(&self, bytes: Vec<u8>) -> SpillChunk {
        SpillChunk {
            sender: self.sender.clone(),
            bytes,
        }
    }

    pub(super) async fn finish(self) -> Option<SpillLog> {
        drop(self.sender);
        self.worker.await.ok()?.ok()
    }
}

pub(super) struct SpillLog {
    path: Option<PathBuf>,
}

impl SpillLog {
    pub(super) fn publish(mut self, jobs: Option<&Jobs>) -> Option<PathBuf> {
        let path = self.path.as_ref()?;
        if let Some(jobs) = jobs
            && !jobs.retain_spill(path)
        {
            return None;
        }
        self.path.take()
    }
}

impl Drop for SpillLog {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn undisclosed_spill_is_removed_on_drop() {
        let spill = Spill::start().unwrap();
        let mut chunk = spill.chunk(b"undisclosed".to_vec());
        chunk.send().await;
        drop(chunk);
        let log = spill.finish().await.unwrap();
        let path = log.path.clone().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"undisclosed");
        drop(log);
        assert!(!path.exists());
    }
}
