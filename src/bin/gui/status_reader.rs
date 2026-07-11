use onedrive_mount::{paths::status_file, status::DaemonStatus};

pub fn read() -> Option<DaemonStatus> {
    DaemonStatus::load(&status_file())
}
