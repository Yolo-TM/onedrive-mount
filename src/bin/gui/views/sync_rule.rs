// Editor for a single sync rule nested inside its remote

use crate::widgets::{interval_input, labeled_field};
use eframe::egui;
use onedrive_mount::{config::SyncRule, conflict::SyncStrategy};

/// Returns `true` when any field changed.
/// `id` must be a stable value (e.g. the rule's index) — NOT the rule name,
/// which changes while the user types and would reset widget state mid-edit.
pub fn show(ui: &mut egui::Ui, rule: &mut SyncRule, id: usize) -> bool {
    let mut changed = false;

    egui::Grid::new(("sync_rule_grid", id))
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

            ui.label("Sync strategy").on_hover_text("How files are synced between local and remote, and what happens when the same file has changed on both sides");
            egui::ComboBox::from_id_salt("sync_strategy")
                .selected_text(rule.sync_strategy.label())
                .show_ui(ui, |ui| {
                    for strategy in SyncStrategy::all() {
                        let (label, tooltip) = strategy_tooltip(strategy);
                        if ui.selectable_value(&mut rule.sync_strategy, *strategy, label)
                            .on_hover_text(tooltip)
                            .clicked()
                        {
                            changed = true;
                        }
                    }
                });
            ui.end_row();

            if rule.sync_strategy.is_destructive() {
                ui.label("");
                ui.colored_label(egui::Color32::YELLOW, "⚠ This strategy is destructive — one side will be overwritten unconditionally.");
                ui.end_row();
            }
        });

    changed
}

fn strategy_tooltip(strategy: &SyncStrategy) -> (&'static str, &'static str) {
    match strategy {
        SyncStrategy::Bidirectional => (
            "Bidirectional",
            "Files are synced both ways.\n\
             On conflict: the local copy is renamed with a timestamp suffix and kept locally.\n\
             The remote version takes the original filename. No data is lost.",
        ),
        SyncStrategy::NewestWins => (
            "Newest wins",
            "Files are synced both ways.\n\
             On conflict: the file with the more recent modification time wins.\n\
             The older version is overwritten — data loss is possible if clocks are skewed.",
        ),
        SyncStrategy::MirrorDown => (
            "Mirror down (remote → local)",
            "One-way: remote is the source of truth.\n\
             Local is made an exact copy of remote on every sync.\n\
             Local-only files are DELETED. Local changes are DISCARDED.",
        ),
        SyncStrategy::MirrorUp => (
            "Mirror up (local → remote)",
            "One-way: local is the source of truth.\n\
             Remote is made an exact copy of local on every sync.\n\
             Remote-only files are DELETED. Remote changes are DISCARDED.",
        ),
    }
}
