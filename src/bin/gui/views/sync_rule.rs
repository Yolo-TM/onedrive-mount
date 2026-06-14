// Editor for a single sync rule nested inside its remote

use crate::widgets::{interval_input, labeled_field};
use eframe::egui;
use onedrive_mount::{config::SyncRule, conflict::ConflictStrategy};

/// Returns `true` when any field changed.
pub fn show(ui: &mut egui::Ui, rule: &mut SyncRule) -> bool {
    let mut changed = false;

    egui::Grid::new(&rule.name)
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            changed |= labeled_field::show(ui, "Name", "docs", &mut rule.name);
            changed |= labeled_field::show(ui, "Remote path", "Files/docs", &mut rule.remote_path);
            changed |= labeled_field::show(ui, "Local path", "~/docs", &mut rule.local_path);

            ui.label("Patterns");
            let mut patterns_str = rule.patterns.join(", ");
            if ui.text_edit_singleline(&mut patterns_str).changed() {
                rule.patterns = patterns_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                changed = true;
            }
            ui.end_row();

            changed |= interval_input::show(ui, "Interval", &mut rule.interval);

            ui.label("Conflict strategy").on_hover_text("What to do when the same file was changed both locally and on the remote since the last sync");
            egui::ComboBox::from_id_salt("conflict_strategy")
                .selected_text(rule.conflict_strategy.label())
                .show_ui(ui, |ui| {
                    for strategy in ConflictStrategy::all() {
                        let (label, tooltip) = conflict_tooltip(strategy);
                        if ui.selectable_value(&mut rule.conflict_strategy, *strategy, label)
                            .on_hover_text(tooltip)
                            .clicked()
                        {
                            changed = true;
                        }
                    }
                });
            ui.end_row();
        });

    changed
}

fn conflict_tooltip(strategy: &ConflictStrategy) -> (&'static str, &'static str) {
    match strategy {
        ConflictStrategy::RemoteWins => (
            "Remote wins",
            "The remote copy always overwrites the local copy.\n\
             Local-only changes will be lost if the remote has a newer version of the same file.",
        ),
        ConflictStrategy::NewestWins => (
            "Newest wins",
            "The file with the more recent modification time is kept.\n\
             Neither copy is safe from being overwritten — whichever is older loses.",
        ),
        ConflictStrategy::KeepBoth => (
            "Keep both",
            "The local conflicting file is renamed with a .conflict-<timestamp> suffix\n\
             and both versions are kept. No data is lost, but duplicates accumulate over time.",
        ),
    }
}
