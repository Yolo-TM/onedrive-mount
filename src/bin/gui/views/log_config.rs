// Log configuration editor plus a live tail of the last N lines of the log file

use crate::widgets::labeled_field;
use eframe::egui;
use onedrive_mount::{config::LogConfig, paths::expand_tilde};
use std::time::SystemTime;

const LOG_LEVELS: &[&str] = &["DEBUG", "INFO", "NOTICE", "ERROR"];
const TAIL_LINES: usize = 30;

/// Cached log tail: only re-read from disk when the file's mtime changes.
pub struct LogTailCache {
    pub content: String,
    last_mtime: Option<SystemTime>,
    last_path: std::path::PathBuf,
}

impl LogTailCache {
    pub fn new() -> Self {
        Self {
            content: String::new(),
            last_mtime: None,
            last_path: std::path::PathBuf::new(),
        }
    }

    /// Refreshes the cache if the file has changed since the last read.
    pub fn refresh(&mut self, path: &std::path::Path) {
        // If the configured path changed, invalidate immediately
        if self.last_path != path {
            self.last_path = path.to_owned();
            self.last_mtime = None;
        }

        let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());

        if mtime == self.last_mtime && self.last_mtime.is_some() {
            return; // file unchanged
        }

        self.last_mtime = mtime;
        self.content = read_tail(path, TAIL_LINES);
    }
}

pub fn show(ui: &mut egui::Ui, log: &mut LogConfig, cache: &mut LogTailCache) -> bool {
    let mut changed = false;

    egui::Grid::new("log_config")
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            changed |= labeled_field::show(
                ui,
                "Log file",
                "~/.local/share/onedrive-mount/daemon.log",
                &mut log.file,
            );

            ui.label("Log level");
            egui::ComboBox::from_id_salt("log_level")
                .selected_text(&log.level)
                .show_ui(ui, |ui| {
                    for level in LOG_LEVELS {
                        if ui
                            .selectable_value(&mut log.level, level.to_string(), *level)
                            .clicked()
                        {
                            changed = true;
                        }
                    }
                });
            ui.end_row();
        });

    ui.add_space(12.0);
    ui.separator();
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(format!("Last {} lines", TAIL_LINES))
            .small()
            .weak(),
    );
    ui.add_space(4.0);

    let path = expand_tilde(&log.file);
    cache.refresh(&path);

    egui::ScrollArea::vertical()
        .id_salt("log_tail")
        .max_height(340.0)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut cache.content.as_str())
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
            );
        });

    changed
}

fn read_tail(path: &std::path::Path, n: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "(log file not found)".into();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
