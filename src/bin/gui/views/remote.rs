// Editor for a single remote: connection settings, mount options, and its sync rules

use crate::widgets::{interval_input, labeled_field};
use eframe::egui;
use onedrive_mount::{config::{RemoteConfig, SyncRule}, status::DaemonStatus};

/// Returns `true` when any field changed.
/// `error` receives any error that should be surfaced to the user (e.g. sync_now failure).
pub fn show(
    ui: &mut egui::Ui,
    remote: &mut RemoteConfig,
    daemon_status: &Option<DaemonStatus>,
    _available_remotes: &[String],
    error: &mut Option<String>,
) -> bool {
    let mut changed = false;

    ui.heading("Remote");
    egui::Grid::new(format!("remote_{}", remote.name))
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            ui.label("rclone remote");
            ui.label(egui::RichText::new(&remote.name).monospace());
            ui.end_row();

            changed |= labeled_field::show(ui, "Mount point", "~/onedrive", &mut remote.mount_point);
            changed |= interval_input::show(ui, "Poll interval", &mut remote.poll_interval);
        });

    ui.add_space(8.0);
    ui.collapsing("Mount options", |ui| {
        egui::Grid::new(format!("mount_{}", remote.name))
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                changed |= labeled_field::show(ui, "VFS cache mode", "full", &mut remote.mount.vfs_cache_mode);
                changed |= labeled_field::show(ui, "VFS cache max age", "72h", &mut remote.mount.vfs_cache_max_age);
                changed |= labeled_field::show(ui, "VFS cache max size", "20G", &mut remote.mount.vfs_cache_max_size);
                changed |= labeled_field::show(ui, "VFS write-back", "5s", &mut remote.mount.vfs_write_back);

                ui.label("Transfers");
                changed |= ui.add(egui::DragValue::new(&mut remote.mount.transfers).range(1..=32)).changed();
                ui.end_row();

                changed |= labeled_field::show(ui, "Dir cache time", "15m", &mut remote.mount.dir_cache_time);

                ui.label("Extra flags");
                let mut extra = remote.mount.extra_flags.join(" ");
                if ui.text_edit_singleline(&mut extra).changed() {
                    remote.mount.extra_flags = extra
                        .split_whitespace()
                        .map(String::from)
                        .collect();
                    changed = true;
                }
                ui.end_row();
            });
    });

    ui.add_space(8.0);
    ui.heading("Sync rules");

    // Look up the current sync status for this remote (for state display + Sync Now)
    let remote_status = daemon_status.as_ref()
        .and_then(|s| s.remotes.iter().find(|r| r.name == remote.name));
    let daemon_pid = daemon_status.as_ref().map(|s| s.pid);

    let mut rule_to_remove: Option<usize> = None;
    let mut sync_now_error: Option<String> = None;
    for (i, rule) in remote.sync_rules.iter_mut().enumerate() {
        ui.push_id(i, |ui| {
            ui.horizontal(|ui| {
                if ui.checkbox(&mut rule.enabled, "").changed() {
                    changed = true;
                }

                // Show sync state badge if daemon is running
                if let Some(rs) = remote_status {
                    if let Some(rule_status) = rs.sync_rules.iter().find(|s| s.name == rule.name) {
                        ui.label(
                            egui::RichText::new(rule_status.state.label())
                                .small()
                                .color(sync_state_color(&rule_status.state))
                        );
                    }
                }

                ui.collapsing(rule.name.clone(), |ui| {
                    changed |= crate::views::sync_rule::show(ui, rule);

                    ui.horizontal(|ui| {
                        if ui.button("Remove rule").clicked() {
                            rule_to_remove = Some(i);
                        }

                        // Sync Now — only available when the daemon is running
                        if let Some(pid) = daemon_pid {
                            if ui.button("⟳ Sync now")
                                .on_hover_text("Trigger an immediate sync for all enabled rules across all remotes")
                                .clicked()
                            {
                                sync_now_error = crate::rclone_query::sync_now(pid).err();
                            }
                        }
                    });
                });
            });
        });
    }
    if let Some(i) = rule_to_remove {
        remote.sync_rules.remove(i);
        changed = true;
    }
    if let Some(e) = sync_now_error {
        *error = Some(e);
    }

    if ui.button("+ Add sync rule").clicked() {
        remote.sync_rules.push(SyncRule {
            name: format!("rule{}", remote.sync_rules.len() + 1),
            remote_path: String::new(),
            local_path: String::new(),
            patterns: vec!["*".into()],
            interval: "15m".into(),
            conflict_strategy: Default::default(),
            enabled: false,
        });
        changed = true;
    }

    changed
}

fn sync_state_color(state: &onedrive_mount::status::SyncState) -> egui::Color32 {
    use onedrive_mount::status::SyncState;
    match state {
        SyncState::Idle => egui::Color32::GRAY,
        SyncState::Running => egui::Color32::YELLOW,
        SyncState::Succeeded => egui::Color32::GREEN,
        SyncState::Failed { .. } => egui::Color32::RED,
    }
}
