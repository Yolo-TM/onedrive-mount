use eframe::egui;

pub fn show(ui: &mut egui::Ui, label: &str, hint: &str, value: &mut String) -> bool {
    ui.label(label);
    let resp = ui.add(egui::TextEdit::singleline(value).hint_text(hint));
    ui.end_row();
    resp.changed()
}
