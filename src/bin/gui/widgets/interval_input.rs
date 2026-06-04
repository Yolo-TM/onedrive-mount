// Duration input that accepts strings like "5m", "30s", "1h" with inline validation

use eframe::egui;

/// Returns `true` when the value changed and parses as a valid interval.
pub fn show(ui: &mut egui::Ui, label: &str, value: &mut String) -> bool {
    ui.label(label);
    let resp = ui.add(
        egui::TextEdit::singleline(value)
            .hint_text("e.g. 5m, 30s, 1h"),
    );
    if !is_valid(value) {
        ui.colored_label(egui::Color32::RED, "invalid (use 30s / 5m / 1h)");
    }
    ui.end_row();
    resp.changed()
}

pub fn is_valid(s: &str) -> bool {
    let s = s.trim();
    (s.ends_with('s') || s.ends_with('m') || s.ends_with('h'))
        && s[..s.len() - 1].parse::<u64>().is_ok()
}
