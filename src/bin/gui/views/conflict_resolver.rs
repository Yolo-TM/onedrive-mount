// Conflict resolution UI: shows pending conflicts and lets the user resolve them.
// Writes decisions to conflict-resolutions.toml which the daemon watches via inotify.

use eframe::egui;
use onedrive_mount::{
    paths::conflict_resolutions_file,
    resolution::{Resolution, ResolutionAction, ResolutionFile},
    status::{ConflictEntry, DaemonStatus},
};

/// Collected conflict with its owning remote/rule names for display.
struct PendingConflict {
    remote: String,
    rule: String,
    entry: ConflictEntry,
}

pub fn show(ui: &mut egui::Ui, status: &Option<DaemonStatus>, error: &mut Option<String>) {
    ui.heading("Conflict Resolution");
    ui.add_space(4.0);

    let conflicts = collect_conflicts(status);

    if conflicts.is_empty() {
        ui.add_space(20.0);
        ui.centered_and_justified(|ui| {
            ui.weak("No pending conflicts.");
        });
        return;
    }

    ui.label(format!(
        "{} conflict(s) pending across {} rule(s)",
        conflicts.len(),
        count_rules(&conflicts),
    ));
    ui.add_space(8.0);

    // Bulk actions
    ui.horizontal(|ui| {
        if ui
            .button("Keep all local")
            .on_hover_text("For every conflict, overwrite remote with the local version")
            .clicked()
        {
            submit_all(&conflicts, ResolutionAction::KeepLocal, error);
        }
        if ui
            .button("Keep all remote")
            .on_hover_text("For every conflict, overwrite local with the remote version")
            .clicked()
        {
            submit_all(&conflicts, ResolutionAction::KeepRemote, error);
        }
        if ui
            .button("Keep both for all")
            .on_hover_text("Rename local copies with .conflict- suffix, pull remote versions")
            .clicked()
        {
            submit_all(&conflicts, ResolutionAction::KeepBoth, error);
        }
    });

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);

    // Per-conflict rows
    for conflict in &conflicts {
        show_conflict_row(ui, conflict, error);
        ui.separator();
    }
}

fn show_conflict_row(ui: &mut egui::Ui, conflict: &PendingConflict, error: &mut Option<String>) {
    let entry = &conflict.entry;

    ui.add_space(4.0);

    // Header
    ui.horizontal(|ui| {
        ui.strong(&entry.file);
        ui.weak(format!(
            "(rule: {}, remote: {})",
            conflict.rule, conflict.remote
        ));
    });

    ui.add_space(2.0);

    // Side-by-side info
    egui::Grid::new(format!(
        "conflict_{}_{}_{}",
        conflict.remote, conflict.rule, entry.file
    ))
    .num_columns(3)
    .spacing([16.0, 4.0])
    .show(ui, |ui| {
        ui.label("");
        ui.strong("Local");
        ui.strong("Remote");
        ui.end_row();

        ui.label("Size");
        ui.label(format_size(entry.local_size));
        ui.label(format_size(entry.remote_size));
        ui.end_row();

        ui.label("Modified");
        ui.label(entry.local_mtime.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string());
        ui.label(entry.remote_mtime.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string());
        ui.end_row();

        ui.label("Path");
        ui.label(egui::RichText::new(&entry.local_path).small().monospace());
        ui.label(egui::RichText::new(&entry.remote_path).small().monospace());
        ui.end_row();
    });

    ui.add_space(2.0);
    ui.weak(format!(
        "Detected: {}",
        entry.detected_at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S")
    ));

    // Try to show a diff for small text files
    show_diff_preview(ui, entry);

    ui.add_space(4.0);

    // Action buttons
    ui.horizontal(|ui| {
        if ui
            .button("Keep local")
            .on_hover_text("Copy local version to remote, discarding remote changes")
            .clicked()
        {
            submit_one(conflict, ResolutionAction::KeepLocal, error);
        }
        if ui
            .button("Keep remote")
            .on_hover_text("Copy remote version to local, discarding local changes")
            .clicked()
        {
            submit_one(conflict, ResolutionAction::KeepRemote, error);
        }
        if ui
            .button("Keep both")
            .on_hover_text("Rename local with .conflict- suffix, pull remote version")
            .clicked()
        {
            submit_one(conflict, ResolutionAction::KeepBoth, error);
        }
    });

    ui.add_space(4.0);
}

fn show_diff_preview(ui: &mut egui::Ui, entry: &ConflictEntry) {
    let local_path = std::path::Path::new(&entry.local_path);

    // Only attempt diff for files under 500KB that look like text
    if entry.local_size > 500 * 1024 || entry.remote_size > 500 * 1024 {
        return;
    }

    let Ok(local_content) = std::fs::read(local_path) else {
        return;
    };

    // Check if content is valid UTF-8
    let Ok(local_text) = std::str::from_utf8(&local_content) else {
        ui.add_space(2.0);
        ui.weak("Binary file — diff not available");
        return;
    };

    if local_text.is_empty() {
        return;
    }

    ui.add_space(4.0);
    ui.collapsing("Local file preview", |ui| {
        egui::ScrollArea::vertical()
            .max_height(200.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new(local_text).monospace().small());
            });
    });
}

fn collect_conflicts(status: &Option<DaemonStatus>) -> Vec<PendingConflict> {
    let Some(status) = status else {
        return vec![];
    };

    let mut conflicts = Vec::new();
    for remote in &status.remotes {
        for rule in &remote.sync_rules {
            for entry in &rule.conflicts {
                conflicts.push(PendingConflict {
                    remote: remote.name.clone(),
                    rule: rule.name.clone(),
                    entry: entry.clone(),
                });
            }
        }
    }
    conflicts
}

fn count_rules(conflicts: &[PendingConflict]) -> usize {
    let mut seen = Vec::new();
    for c in conflicts {
        let key = (&c.remote, &c.rule);
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    seen.len()
}

fn submit_all(conflicts: &[PendingConflict], action: ResolutionAction, error: &mut Option<String>) {
    let resolutions: Vec<Resolution> = conflicts
        .iter()
        .map(|c| Resolution {
            remote: c.remote.clone(),
            rule: c.rule.clone(),
            file: c.entry.file.clone(),
            action: action.clone(),
            resolved_at: chrono::Utc::now(),
        })
        .collect();

    write_resolutions(resolutions, error);
}

fn submit_one(conflict: &PendingConflict, action: ResolutionAction, error: &mut Option<String>) {
    let resolution = Resolution {
        remote: conflict.remote.clone(),
        rule: conflict.rule.clone(),
        file: conflict.entry.file.clone(),
        action,
        resolved_at: chrono::Utc::now(),
    };

    write_resolutions(vec![resolution], error);
}

fn write_resolutions(resolutions: Vec<Resolution>, error: &mut Option<String>) {
    let path = conflict_resolutions_file();

    // Load existing resolutions and append new ones (in case daemon hasn't processed previous batch)
    let mut file = ResolutionFile::load(&path).unwrap_or_default();
    file.resolutions.extend(resolutions);

    if let Err(e) = file.save(&path) {
        *error = Some(format!("Failed to write resolutions: {e}"));
    }
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
