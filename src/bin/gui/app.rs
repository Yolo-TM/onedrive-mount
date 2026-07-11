use crate::{
    config_io,
    rclone_config_wizard::Wizard,
    rclone_query,
    state::State,
    status_reader, systemd,
    views::{conflict_resolver, log_config, remote, service, status, wizard},
};
use eframe::egui;
use onedrive_mount::config::RemoteConfig;
use onedrive_mount::status::MountState;
use std::time::{Duration, Instant};

#[derive(PartialEq, Clone)]
enum Nav {
    Status,
    Remotes,
    Logging,
    Conflicts,
}

pub struct App {
    state: State,
    nav: Nav,
    did_startup: bool,
    daemon_starting: bool,
    _pid_lock: onedrive_mount::pid_lock::PidLock,
}

impl App {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        pid_lock: onedrive_mount::pid_lock::PidLock,
        resolve_conflicts: bool,
    ) -> Self {
        Self {
            state: State::new(),
            nav: if resolve_conflicts {
                Nav::Conflicts
            } else {
                Nav::Status
            },
            did_startup: false,
            daemon_starting: false,
            _pid_lock: pid_lock,
        }
    }
}

const TOAST_DURATION: Duration = Duration::from_secs(3);

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.state.poll_remotes_loading();

        if !self.did_startup {
            self.did_startup = true;
            self.state.daemon_active = systemd::is_active();
            self.state.service_enabled = systemd::is_enabled();
            if self.state.service_enabled && !self.state.daemon_active {
                match systemd::start() {
                    Ok(()) => self.daemon_starting = true,
                    Err(e) => self.state.service_error = Some(e),
                }
            }
        }

        let poll_interval = if self.daemon_starting {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(2)
        };

        if self.state.last_status_poll.elapsed() > poll_interval {
            self.state.status = status_reader::read();
            self.state.daemon_active = systemd::is_active();
            self.state.service_enabled = systemd::is_enabled();
            let log_path = onedrive_mount::paths::expand_tilde(&self.state.config.log.file);
            self.state.log_tail.refresh(&log_path);
            self.state.last_status_poll = Instant::now();
            if self.daemon_starting && self.state.daemon_active {
                self.daemon_starting = false;
            }
            if self.state.service_enabled
                && !self.state.daemon_active
                && !self.daemon_starting
                && let Some(err) = systemd::last_exit_error()
                && self.state.service_error.is_none()
            {
                self.state.service_error = Some(format!("Daemon stopped: {err}"));
            }
        }
        ctx.request_repaint_after(poll_interval);

        if let Some((_, ts)) = &self.state.save_toast
            && ts.elapsed() > TOAST_DURATION
        {
            self.state.save_toast = None;
        }

        if self.state.wizard.is_some() {
            show_wizard_modal(&ctx, ui, &mut self.state);
        }

        egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                let active = self.state.daemon_active;
                let (color, daemon_label) = if active {
                    let pid_str = self.state.status.as_ref()
                        .map(|s| format!("● Daemon running (pid {})", s.pid))
                        .unwrap_or_else(|| "● Daemon running".into());
                    (egui::Color32::GREEN, pid_str)
                } else if self.daemon_starting {
                    (egui::Color32::YELLOW, "● Daemon starting…".into())
                } else {
                    (egui::Color32::RED, "● Daemon stopped".into())
                };
                ui.colored_label(color, daemon_label);

                if let Some(status) = &self.state.status {
                    let now = chrono::Utc::now();
                    for remote in &status.remotes {
                        let (pill_color, pill_label): (egui::Color32, String) = match &remote.mount {
                            MountState::Mounting => (egui::Color32::YELLOW, "…".into()),
                            MountState::Failed { .. } => (egui::Color32::RED, "✗".into()),
                            _ => continue,
                        };
                        let hover = match &remote.mount {
                            MountState::Mounting => {
                                if let Some(started) = status.started_at {
                                    let secs = (now - started).num_seconds().max(0);
                                    format!("{}: Mounting ({}s)", remote.name, secs)
                                } else {
                                    format!("{}: Mounting…", remote.name)
                                }
                            }
                            MountState::Failed { error, at } => {
                                let local_at = at.with_timezone(&chrono::Local);
                                format!("{}: Failed at {}\n{}", remote.name, local_at.format("%H:%M:%S"), error)
                            }
                            _ => format!("{}: {}", remote.name, mount_state_label(&remote.mount)),
                        };
                        ui.separator();
                        ui.colored_label(pill_color, &pill_label).on_hover_text(&hover);
                        ui.label(&remote.name);
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    service::show_controls(ui, self.state.service_enabled, &mut self.state.service_error);

                    if let Some(cfg_err) = self.state.status.as_ref().and_then(|s| s.config_error.as_ref()) {
                        ui.colored_label(egui::Color32::RED, format!("⚠ Config error: {cfg_err}"))
                            .on_hover_text("The daemon detected an invalid config file and kept its previous configuration. Fix config.toml and save again.");
                    }

                    if let Some(err) = &self.state.service_error {
                        let resp = ui.colored_label(egui::Color32::RED, err);
                        if resp.clicked() {
                            self.state.service_error = None;
                        }
                        resp.on_hover_text("Click to dismiss");
                    }

                    if let Some((msg, _)) = &self.state.save_toast {
                        ui.colored_label(egui::Color32::GREEN, msg);
                    }

                    ui.separator();

                    let save_btn = if self.state.config_dirty {
                        ui.button(egui::RichText::new("Save *").strong())
                    } else {
                        ui.add_enabled(false, egui::Button::new("Save"))
                    };
                    if save_btn.clicked() {
                        match config_io::save(&self.state.config) {
                            Ok(()) => {
                                self.state.config_dirty = false;
                                let msg = if self.state.daemon_active {
                                    "✓ Saved — daemon reloading"
                                } else {
                                    "✓ Saved (daemon not running)"
                                };
                                self.state.save_toast = Some((msg.into(), Instant::now()));
                            }
                            Err(e) => self.state.service_error = Some(e),
                        }
                    }
                });
            });
        });

        egui::Panel::left("nav").show_inside(ui, |ui| {
            ui.heading("onedrive-mount");
            ui.separator();

            ui.selectable_value(&mut self.nav, Nav::Status, "Status");
            ui.selectable_value(&mut self.nav, Nav::Remotes, "Remotes");
            ui.selectable_value(&mut self.nav, Nav::Logging, "Logging");

            let conflict_count = self
                .state
                .status
                .as_ref()
                .map(|s| {
                    s.remotes
                        .iter()
                        .flat_map(|r| &r.sync_rules)
                        .map(|sr| sr.conflicts.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            if conflict_count > 0 {
                let label = format!("⚠ Conflicts ({})", conflict_count);
                ui.selectable_value(
                    &mut self.nav,
                    Nav::Conflicts,
                    egui::RichText::new(label).color(egui::Color32::from_rgb(255, 165, 0)),
                );
            } else {
                ui.selectable_value(&mut self.nav, Nav::Conflicts, "Conflicts");
            }
        });

        egui::CentralPanel::default().show_inside(ui, |ui| match self.nav {
            Nav::Status => {
                status::show(
                    ui,
                    &self.state.config,
                    &self.state.status,
                    self.state.daemon_active,
                );
            }
            Nav::Remotes => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    show_remotes(ui, &mut self.state);
                });
            }
            Nav::Logging => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if log_config::show(ui, &mut self.state.config.log, &mut self.state.log_tail) {
                        self.state.config_dirty = true;
                    }
                });
            }
            Nav::Conflicts => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    conflict_resolver::show(ui, &self.state.status, &mut self.state.service_error);
                });
            }
        });
    }
}

fn show_wizard_modal(ctx: &egui::Context, _ui: &mut egui::Ui, state: &mut State) {
    let modal = egui::Modal::new(egui::Id::new("rclone_wizard")).show(ctx, |ui| {
        ui.set_min_width(480.0);

        let Some(w) = state.wizard.as_mut() else {
            return;
        };

        let done = wizard::show(ui, w);
        if done {
            let remotes = rclone_query::list_remotes();
            state.available_remotes = remotes;
            state.wizard = None;
            return;
        }

        ui.add_space(16.0);
        ui.separator();
        if ui.button("✕ Cancel").clicked() {
            if let Some(w) = &state.wizard
                && w.step != crate::rclone_config_wizard::WizardStep::Init
                && !w.remote_name.is_empty()
            {
                let name = w.remote_name.clone();
                state.wizard = None;
                if let Err(e) = rclone_query::delete_remote(&name) {
                    state.service_error = Some(format!(
                        "Cancelled — could not clean up partial remote '{name}': {e}"
                    ));
                } else {
                    state.available_remotes = rclone_query::list_remotes();
                }
                return;
            }
            state.wizard = None;
        }
    });
    if modal.should_close() {
        state.wizard = None;
    }
}

fn show_remotes(ui: &mut egui::Ui, state: &mut State) {
    if !state.config.remotes.is_empty() {
        ui.label(egui::RichText::new("Configured remotes").small().weak());
        ui.add_space(2.0);

        let mut remove_idx: Option<usize> = None;
        for i in 0..state.config.remotes.len() {
            let selected = state.selected_remote == Some(i);
            let name = state.config.remotes[i].name.clone();

            ui.horizontal(|ui| {
                let enabled = &mut state.config.remotes[i].enabled;
                if ui.checkbox(enabled, "").changed() {
                    state.config_dirty = true;
                }
                if ui.selectable_label(selected, &name).clicked() {
                    state.selected_remote = if selected { None } else { Some(i) };
                }
                if selected
                    && ui
                        .small_button("Remove")
                        .on_hover_text("Remove from app config (does not delete the rclone remote)")
                        .clicked()
                {
                    remove_idx = Some(i);
                }
            });

            if selected {
                ui.indent(format!("remote_editor_{i}"), |ui| {
                    if remote::show(
                        ui,
                        &mut state.config.remotes[i],
                        i,
                        &state.status,
                        &mut state.service_error,
                    ) {
                        state.config_dirty = true;
                    }
                });
                ui.add_space(4.0);
            }
        }

        if let Some(idx) = remove_idx {
            state.config.remotes.remove(idx);
            state.selected_remote = None;
            state.config_dirty = true;
        }

        ui.add_space(8.0);
    }

    let configured_names: Vec<_> = state
        .config
        .remotes
        .iter()
        .map(|r| r.name.clone())
        .collect();
    let untracked: Vec<_> = state
        .available_remotes
        .iter()
        .filter(|n| !configured_names.contains(n))
        .cloned()
        .collect();

    if !untracked.is_empty() {
        ui.label(
            egui::RichText::new("rclone remotes (not in app config)")
                .small()
                .weak(),
        );
        ui.add_space(2.0);

        let mut to_add: Option<String> = None;
        let mut to_delete: Option<String> = None;
        for name in &untracked {
            ui.horizontal(|ui| {
                ui.label(name);
                if ui
                    .small_button("Add to config")
                    .on_hover_text("Add mount and sync settings for this remote")
                    .clicked()
                {
                    to_add = Some(name.clone());
                }
                if ui
                    .small_button("Delete")
                    .on_hover_text("Permanently remove from rclone")
                    .clicked()
                {
                    to_delete = Some(name.clone());
                }
            });
        }
        if let Some(name) = to_add {
            state.config.remotes.push(RemoteConfig {
                name: name.clone(),
                r#type: "onedrive".into(),
                mount_point: format!("~/{name}"),
                poll_interval: onedrive_mount::defaults::poll_interval(),
                enabled: true,
                mount: Default::default(),
                sync_rules: vec![],
            });
            state.selected_remote = Some(state.config.remotes.len() - 1);
            state.config_dirty = true;
        }
        if let Some(name) = to_delete {
            if let Err(e) = rclone_query::delete_remote(&name) {
                state.service_error = Some(e);
            } else {
                state.available_remotes = rclone_query::list_remotes();
            }
        }
        ui.add_space(8.0);
    }

    if state.available_remotes.is_empty() && state.config.remotes.is_empty() {
        ui.weak("Loading rclone remotes…");
        ui.add_space(4.0);
    }

    ui.separator();
    if ui
        .button("Setup new remote…")
        .on_hover_text("Create and authenticate a new rclone remote without leaving the app")
        .clicked()
    {
        state.wizard = Some(Wizard::new());
    }
}

fn mount_state_label(state: &MountState) -> &'static str {
    match state {
        MountState::Unmounted => "Unmounted",
        MountState::Mounting => "Mounting",
        MountState::Mounted { .. } => "Mounted",
        MountState::Failed { .. } => "Failed",
    }
}
