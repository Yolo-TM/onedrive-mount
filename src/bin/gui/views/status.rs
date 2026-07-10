// Overview page: per-remote mount status, mount point directory listing, and sync rule states

use eframe::egui;
use onedrive_mount::{
    config::{Config, RemoteConfig},
    paths::expand_tilde,
    status::{DaemonStatus, MountState, SyncState},
};
use std::path::PathBuf;

pub fn show(
    ui: &mut egui::Ui,
    config: &Config,
    status: &Option<DaemonStatus>,
    daemon_active: bool,
) {
    if !daemon_active {
        ui.centered_and_justified(|ui| {
            ui.weak("Daemon is not running — start it from the service controls below.");
        });
        return;
    }

    let Some(status) = status else {
        ui.centered_and_justified(|ui| {
            ui.weak("Waiting for daemon status…");
        });
        return;
    };

    // Version header
    ui.horizontal(|ui| {
        ui.weak(format!("GUI v{}", env!("CARGO_PKG_VERSION")));
        ui.separator();
        if status.version.is_empty() {
            ui.weak("Daemon v?");
        } else {
            ui.weak(format!("Daemon v{}", status.version));
            if status.version != env!("CARGO_PKG_VERSION") {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "⚠ version mismatch — reinstall recommended",
                );
            }
        }
    });
    ui.add_space(4.0);

    if config.remotes.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.weak("No remotes configured. Add one in the Remotes tab.");
        });
        return;
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        for remote_cfg in &config.remotes {
            let remote_status = status.remotes.iter().find(|r| r.name == remote_cfg.name);
            show_remote(ui, remote_cfg, remote_status);
            ui.add_space(12.0);
        }
    });
}

fn show_remote(
    ui: &mut egui::Ui,
    cfg: &RemoteConfig,
    status: Option<&onedrive_mount::status::RemoteStatus>,
) {
    let mount_state = status.map(|s| &s.mount);

    // Header row: remote name + mount state badge
    ui.horizontal(|ui| {
        ui.heading(&cfg.name);
        ui.add_space(8.0);
        match mount_state {
            None | Some(MountState::Unmounted) => {
                ui.colored_label(egui::Color32::GRAY, "● Unmounted");
            }
            Some(MountState::Mounting) => {
                ui.colored_label(egui::Color32::YELLOW, "● Mounting…");
            }
            Some(MountState::Mounted { since }) => {
                let local_since = since.with_timezone(&chrono::Local);
                let label = format!("● Mounted since {}", local_since.format("%H:%M:%S"));
                ui.colored_label(egui::Color32::GREEN, label);
            }
            Some(MountState::Failed { error, at }) => {
                let local_at = at.with_timezone(&chrono::Local);
                ui.colored_label(egui::Color32::RED, "● Failed")
                    .on_hover_text(format!("at {}\n{}", local_at.format("%H:%M:%S"), error));
            }
        }
    });

    let mount_point = expand_tilde(&cfg.mount_point);
    ui.label(
        egui::RichText::new(mount_point.display().to_string())
            .small()
            .weak()
            .monospace(),
    );

    ui.add_space(4.0);

    // Directory listing — only meaningful when mounted
    let is_mounted = matches!(mount_state, Some(MountState::Mounted { .. }));
    if is_mounted {
        show_dir_listing(ui, &mount_point);
    } else {
        ui.weak("(not mounted)");
    }

    // Sync rules
    if let Some(rs) = status
        && !rs.sync_rules.is_empty()
    {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Sync rules").small().strong());
        for rule in &rs.sync_rules {
            ui.horizontal(|ui| {
                let (color, label) = match &rule.state {
                    SyncState::Idle => (egui::Color32::GRAY, "Idle".to_string()),
                    SyncState::Running => (egui::Color32::YELLOW, "Running".to_string()),
                    SyncState::Succeeded => (egui::Color32::GREEN, "OK".to_string()),
                    SyncState::Failed { error, .. } => {
                        (egui::Color32::RED, format!("Failed: {error}"))
                    }
                    SyncState::BlockedOnConflicts { .. } => (
                        egui::Color32::from_rgb(255, 165, 0),
                        format!("⚠ {} conflict(s)", rule.conflicts.len()),
                    ),
                };
                ui.colored_label(color, "●");
                ui.label(&rule.name);
                ui.weak("—");
                ui.label(egui::RichText::new(&label).small());

                if let Some(last) = rule.last_sync {
                    let local_last = last.with_timezone(&chrono::Local);
                    ui.weak(format!("last: {}", local_last.format("%H:%M:%S")));
                }
                if let Some(next) = rule.next_sync {
                    let local_next = next.with_timezone(&chrono::Local);
                    ui.weak(format!("next: {}", local_next.format("%H:%M:%S")));
                }

                // Show transfer stats from last successful sync
                if let Some(files) = rule.files_transferred {
                    let bytes_str = rule.bytes_transferred.map(format_size).unwrap_or_default();
                    ui.weak(format!("{files} file(s), {bytes_str}"));
                }
            });
        }
    }

    ui.separator();
}

fn show_dir_listing(ui: &mut egui::Ui, path: &PathBuf) {
    let entries = read_dir_entries(path);

    if entries.is_empty() {
        ui.weak("(empty)");
        return;
    }

    // Show up to 32 entries — enough to be useful without flooding the panel
    const MAX_ENTRIES: usize = 32;
    let shown = entries.len().min(MAX_ENTRIES);

    egui::Grid::new(path.display().to_string())
        .num_columns(2)
        .spacing([12.0, 2.0])
        .show(ui, |ui| {
            for entry in &entries[..shown] {
                let icon = if entry.is_dir { "📁" } else { "📄" };
                ui.label(format!("{} {}", icon, entry.name));
                if let Some(size) = entry.size {
                    ui.label(
                        egui::RichText::new(format_size(size))
                            .small()
                            .weak()
                            .monospace(),
                    );
                } else {
                    ui.label("");
                }
                ui.end_row();
            }
        });

    if entries.len() > MAX_ENTRIES {
        ui.weak(format!("… and {} more", entries.len() - MAX_ENTRIES));
    }
}

struct DirEntry {
    name: String,
    is_dir: bool,
    size: Option<u64>,
}

fn read_dir_entries(path: &PathBuf) -> Vec<DirEntry> {
    let Ok(rd) = std::fs::read_dir(path) else {
        return vec![];
    };

    let mut entries: Vec<DirEntry> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            // Skip hidden files
            if name.starts_with('.') {
                return None;
            }
            let meta = e.metadata().ok()?;
            let is_dir = meta.is_dir();
            let size = if is_dir { None } else { Some(meta.len()) };
            Some(DirEntry { name, is_dir, size })
        })
        .collect();

    // Dirs first, then files, both alphabetical
    entries.sort_unstable_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));

    entries
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
