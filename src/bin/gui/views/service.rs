use crate::systemd;
use eframe::egui;

pub fn show_controls(ui: &mut egui::Ui, service_enabled: bool, error: &mut Option<String>) {
    if service_enabled {
        if ui
            .small_button("Remove service")
            .on_hover_text("Disable and remove the systemd user service")
            .clicked()
        {
            match systemd::uninstall() {
                Ok(()) => *error = None,
                Err(e) => *error = Some(e),
            }
        }
    } else {
        if ui
            .small_button("Install service")
            .on_hover_text(
                "Install and enable the systemd user service so the daemon starts automatically",
            )
            .clicked()
        {
            match systemd::install() {
                Ok(()) => *error = None,
                Err(e) => *error = Some(e),
            }
        }
    }
}
