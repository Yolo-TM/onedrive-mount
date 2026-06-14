// Step-by-step rclone remote creation wizard — answers rclone's JSON questions in-app

use crate::rclone_config_wizard::{RcloneExample, RcloneOption, Wizard, WizardStep};
use eframe::egui;

const KNOWN_TYPES: &[(&str, &str)] = &[
    ("onedrive", "Microsoft OneDrive"),
    ("drive", "Google Drive"),
    ("dropbox", "Dropbox"),
    ("box", "Box"),
    ("s3", "Amazon S3"),
    ("b2", "Backblaze B2"),
    ("sftp", "SFTP"),
    ("ftp", "FTP"),
];

/// Returns `true` when the wizard completed and the remote list should be refreshed.
pub fn show(ui: &mut egui::Ui, wizard: &mut Wizard) -> bool {
    wizard.poll();
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(200));

    match wizard.step.clone() {
        WizardStep::Init => show_init(ui, wizard),
        WizardStep::Working => show_working(ui),
        WizardStep::Question(q) => {
            if let Some(opt) = q.option {
                show_question(ui, wizard, &opt)
            } else {
                // Option was null but state non-empty — treat as done
                wizard.step = WizardStep::Done;
                false
            }
        }
        WizardStep::WaitingOAuth { url } => show_oauth(ui, &url),
        WizardStep::Done => show_done(ui),
        WizardStep::Error(ref e) => show_error(ui, wizard, &e.clone()),
    }
}

fn show_working(ui: &mut egui::Ui) -> bool {
    ui.heading("Working…");
    ui.add_space(8.0);
    ui.spinner();
    false
}

fn show_init(ui: &mut egui::Ui, wizard: &mut Wizard) -> bool {
    ui.heading("Add rclone remote");
    ui.add_space(8.0);

    egui::Grid::new("wizard_init")
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            ui.label("Remote name");
            ui.text_edit_singleline(&mut wizard.remote_name);
            ui.end_row();

            ui.label("Provider");
            egui::ComboBox::from_id_salt("remote_type")
                .selected_text(type_label(&wizard.remote_type))
                .show_ui(ui, |ui| {
                    for (value, label) in KNOWN_TYPES {
                        ui.selectable_value(&mut wizard.remote_type, value.to_string(), *label);
                    }
                });
            ui.end_row();
        });

    ui.add_space(8.0);

    let name_valid = is_valid_remote_name(&wizard.remote_name);
    if !wizard.remote_name.is_empty() && !name_valid {
        ui.colored_label(
            egui::Color32::RED,
            "Name must contain only letters, digits, hyphens, and underscores.",
        );
    }

    if ui
        .add_enabled(name_valid, egui::Button::new("Next →"))
        .clicked()
    {
        wizard.start();
    }
    false
}

fn show_question(ui: &mut egui::Ui, wizard: &mut Wizard, opt: &RcloneOption) -> bool {
    ui.heading(format!("Configure: {}", wizard.remote_name));
    ui.add_space(4.0);

    for line in opt.help.trim().lines() {
        ui.label(line);
    }
    ui.add_space(8.0);

    // Yes/No exclusive questions: render as buttons that submit immediately on click
    if let Some(ref examples) = opt.examples {
        if is_bool_exclusive(opt, examples) {
            ui.horizontal(|ui| {
                for ex in examples {
                    let label = if ex.help.is_empty() {
                        ex.value.as_str()
                    } else {
                        ex.help.as_str()
                    };
                    if ui.button(label).clicked() {
                        wizard.current_answer = ex.value.clone();
                        wizard.submit_answer();
                    }
                }
                if wizard.can_go_back() && ui.button("← Back").clicked() {
                    wizard.go_back();
                }
            });
            return false;
        }

        show_combo(ui, wizard, examples, opt.exclusive);
    } else if opt.is_password {
        ui.add(egui::TextEdit::singleline(&mut wizard.current_answer).password(true));
    } else {
        ui.text_edit_singleline(&mut wizard.current_answer);
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if wizard.can_go_back() && ui.button("← Back").clicked() {
            wizard.go_back();
        }
        if ui.button("Next →").clicked() {
            wizard.submit_answer();
        }
        if !opt.required
            && ui
                .button("Skip")
                .on_hover_text("Use the default value")
                .clicked()
        {
            wizard.current_answer.clear();
            wizard.submit_answer();
        }
    });
    false
}

fn is_bool_exclusive(opt: &RcloneOption, examples: &[RcloneExample]) -> bool {
    opt.exclusive
        && examples.len() == 2
        && examples
            .iter()
            .any(|e| e.value.eq_ignore_ascii_case("true") || e.value == "Yes")
        && examples
            .iter()
            .any(|e| e.value.eq_ignore_ascii_case("false") || e.value == "No")
}

fn show_combo(ui: &mut egui::Ui, wizard: &mut Wizard, opts: &[RcloneExample], exclusive: bool) {
    let selected_label = opts
        .iter()
        .find(|ex| ex.value == wizard.current_answer)
        .map(|ex| {
            if ex.help.is_empty() {
                ex.value.as_str()
            } else {
                ex.help.as_str()
            }
        })
        .unwrap_or(wizard.current_answer.as_str());

    egui::ComboBox::from_id_salt("wizard_choice")
        .selected_text(selected_label)
        .show_ui(ui, |ui| {
            for ex in opts {
                let label = if ex.help.is_empty() {
                    ex.value.as_str()
                } else {
                    ex.help.as_str()
                };
                ui.selectable_value(&mut wizard.current_answer, ex.value.clone(), label);
            }
        });

    // Non-exclusive: allow free-text override below the combo
    if !exclusive {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Or enter a value manually:")
                .small()
                .weak(),
        );
        ui.text_edit_singleline(&mut wizard.current_answer);
    }
}

fn show_oauth(ui: &mut egui::Ui, url: &str) -> bool {
    ui.heading("Browser authentication required");
    ui.add_space(8.0);
    ui.label("Complete the login in your browser. If it didn't open automatically, visit:");
    ui.add_space(4.0);
    ui.label(egui::RichText::new(url).monospace());
    ui.add_space(8.0);
    ui.spinner();
    ui.add_space(8.0);
    if ui.button("Open in browser").clicked() {
        let _ = open::that(url);
    }
    false
}

fn show_done(ui: &mut egui::Ui) -> bool {
    ui.heading("Remote created successfully");
    ui.add_space(8.0);
    ui.label("The remote has been added to your rclone config. You can now select it in the Remotes tab.");
    ui.add_space(8.0);
    true
}

fn show_error(ui: &mut egui::Ui, wizard: &mut Wizard, error: &str) -> bool {
    ui.heading("Something went wrong");
    ui.add_space(8.0);
    ui.colored_label(egui::Color32::RED, error);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if wizard.can_go_back() && ui.button("← Back").clicked() {
            wizard.go_back();
        }
        if ui.button("Start over").clicked() {
            wizard.step = WizardStep::Init;
            wizard.current_answer.clear();
        }
    });
    false
}

fn type_label(t: &str) -> &str {
    KNOWN_TYPES
        .iter()
        .find(|(v, _)| *v == t)
        .map(|(_, l)| *l)
        .unwrap_or(t)
}

/// rclone remote names must be non-empty and contain only alphanumeric characters,
/// hyphens, and underscores. Spaces and shell metacharacters are not allowed.
fn is_valid_remote_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}
