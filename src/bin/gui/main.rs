mod app;
mod config_io;
mod rclone_config_wizard;
mod rclone_query;
mod state;
mod status_reader;
mod systemd;
mod views;
mod widgets;

fn main() -> eframe::Result {
    use onedrive_mount::{paths::gui_pid_file, pid_lock::PidLock};
    let pid_lock = match PidLock::acquire(&gui_pid_file()) {
        Ok(lock) => lock,
        Err(pid) => {
            eprintln!("onedrive-mount is already running (pid {pid})");
            std::process::exit(1);
        }
    };

    // Print version if requested
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("onedrive-mount {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // winit prefers Wayland when WAYLAND_DISPLAY is set, but the Wayland libs may not be
    // available in the dynamic linker path (common on NixOS). Force X11 so winit uses the
    // statically-discovered libX11 instead of trying to dlopen libwayland-client.
    // Safety: called before any threads are spawned, so no other thread can observe the env.
    unsafe {
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("WAYLAND_SOCKET");
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("onedrive-mount")
            .with_inner_size([900.0, 600.0])
            .with_icon(load_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "onedrive-mount",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, pid_lock)))),
    )
}

fn load_icon() -> std::sync::Arc<eframe::egui::viewport::IconData> {
    const SVG: &[u8] = include_bytes!("../../../assets/icon.svg");
    const SIZE: u32 = 64;

    let tree = resvg::usvg::Tree::from_data(SVG, &resvg::usvg::Options::default())
        .expect("icon.svg is valid SVG");

    let mut pixmap = resvg::tiny_skia::Pixmap::new(SIZE, SIZE).unwrap();
    let scale = SIZE as f32 / tree.size().width().max(tree.size().height());
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    std::sync::Arc::new(eframe::egui::viewport::IconData {
        rgba: pixmap.take(),
        width: SIZE,
        height: SIZE,
    })
}
