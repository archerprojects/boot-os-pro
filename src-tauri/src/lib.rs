mod commands;
mod disk;
mod error;
mod grub;
mod manifest;
mod squash;
mod theme;

// Manager provides get_webview_window, used in the debug devtools setup block.
#[cfg(debug_assertions)]
use tauri::Manager;

pub fn run() {
    tracing_subscriber::fmt::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            commands::list_devices,
            commands::run_full_write,
            commands::add_persistent_iso,
            commands::add_live_iso,
            commands::format_persistent_slot,
            commands::format_free_space_slot,
            commands::read_drive_manifest,
            commands::reconcile_manifest,
            commands::cancel_operation,
            commands::get_partition_layout,
            commands::format_partition,
            commands::delete_partition,
            commands::wipe_device,
            commands::unmount_partition,
            commands::get_file_size,
            commands::get_iso_arch,
            commands::get_theme_colors,
        ])
        .setup(|_app| {
            #[cfg(debug_assertions)]
            _app.get_webview_window("main").unwrap().open_devtools();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error running Boot OS Pro");
}
