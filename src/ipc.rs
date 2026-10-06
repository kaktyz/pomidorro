use crate::model::{Settings, Snapshot};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::OnceLock,
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Status,
    Toggle,
    Skip,
    Reset,
    ToggleDim,
    ToggleSound,
    TestNotification,
    Settings(Settings),
    Shutdown,
}

#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub snapshot: Snapshot,
    pub error: Option<String>,
}

pub fn socket_path() -> PathBuf {
    static SOCKET: OnceLock<PathBuf> = OnceLock::new();
    SOCKET.get_or_init(resolve_socket_path).clone()
}

fn resolve_socket_path() -> PathBuf {
    // A short per-user path avoids Darwin's 104-byte Unix socket path limit.
    let uid = std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "user".into()));
    PathBuf::from(format!("/tmp/pomidorro-{uid}/service.sock"))
}

pub fn request(command: &Command) -> Result<Snapshot> {
    let mut stream = UnixStream::connect(socket_path()).context("Фоновый процесс недоступен")?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    serde_json::to_writer(&mut stream, command)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).take_line(&mut line)?;
    let reply: Reply = serde_json::from_str(&line)?;
    if let Some(error) = reply.error {
        anyhow::bail!(error);
    }
    Ok(reply.snapshot)
}

trait LimitedLine: BufRead {
    fn take_line(&mut self, line: &mut String) -> Result<()> {
        use std::io::Read;
        let count = self.take(65536).read_line(line)?;
        anyhow::ensure!(count > 0 && line.ends_with('\n'), "Неполный ответ сервера");
        Ok(())
    }
}
impl<T: BufRead> LimitedLine for T {}
