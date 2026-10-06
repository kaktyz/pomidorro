mod ipc;
mod model;
#[cfg(target_os = "macos")]
mod service;
mod storage;
mod tui;

use anyhow::Result;

fn main() -> Result<()> {
    if std::env::args().any(|arg| arg == "--service") {
        #[cfg(target_os = "macos")]
        return service::run();
        #[cfg(not(target_os = "macos"))]
        anyhow::bail!("Фоновый процесс поддерживает только macOS");
    }
    if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Pomidorro — помидорный таймер для macOS\n\nЗапуск: pomidorro\nУправление внутри TUI: Space — старт/пауза, F1 — помощь, F10 — выход."
        );
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("Pomidorro предназначен для macOS");
    #[cfg(target_os = "macos")]
    tui::run()
}
