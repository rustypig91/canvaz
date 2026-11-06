//! Configure the distribution before Tauri resolves paths or creates webviews.

pub fn context<R: tauri::Runtime>() -> tauri::Context<R> {
    let mut context = tauri::generate_context!();
    if cfg!(feature = "microsoft-store") {
        // This is Tauri's storage/application identifier, not the Partner Center
        // MSIX package identity. Keep it stable across Store package upgrades.
        // Tauri uses it for every app path and its default Windows/Linux webview
        // data directory, so plugins and browser storage follow the same edition.
        context.config_mut().identifier = "com.christoffer.canvaz.store".into();
        // The MSIX contains this pinned runtime beside canvaz.exe. Set it here
        // before Tauri creates WebView2, independently of the host's Evergreen
        // installation. The build script stages the matching directory.
        context.config_mut().bundle.windows.webview_install_mode =
            tauri::utils::config::WebviewInstallMode::FixedRuntime { path: "WebView2".into() };
    }
    context
}

#[cfg(test)]
mod tests {
    use tauri::test::{mock_builder, MockRuntime};
    use tauri::Manager;

    fn app(mut context: tauri::Context<MockRuntime>) -> tauri::App<MockRuntime> {
        // Resolve production paths without opening a webview or writing to the
        // real user profile. The MockRuntime needs no display or CAN hardware.
        context.config_mut().app.windows.clear();
        mock_builder().build(context).unwrap()
    }

    #[test]
    fn edition_paths_preserve_the_direct_installation_and_isolate_store_storage() {
        let direct = app(tauri::generate_context!());
        let edition = app(super::context());
        assert_eq!(direct.config().identifier, "com.christoffer.canvaz");
        if cfg!(feature = "microsoft-store") {
            assert!(matches!(
                &edition.config().bundle.windows.webview_install_mode,
                tauri::utils::config::WebviewInstallMode::FixedRuntime { path }
                    if path == std::path::Path::new("WebView2")
            ));
        }

        let direct_paths = [
            direct.path().app_data_dir().unwrap(),
            direct.path().app_local_data_dir().unwrap(),
            direct.path().app_config_dir().unwrap(),
            direct.path().app_cache_dir().unwrap(),
            direct.path().app_log_dir().unwrap(),
        ];
        let edition_paths = [
            edition.path().app_data_dir().unwrap(),
            edition.path().app_local_data_dir().unwrap(),
            edition.path().app_config_dir().unwrap(),
            edition.path().app_cache_dir().unwrap(),
            edition.path().app_log_dir().unwrap(),
        ];
        for (direct, edition) in direct_paths.iter().zip(&edition_paths) {
            if cfg!(feature = "microsoft-store") {
                assert_ne!(direct, edition);
                assert!(!edition.starts_with(direct));
            } else {
                assert_eq!(direct, edition);
            }
        }

        // Tauri's Windows/Linux webview preparation uses this exact resolution.
        let webview_profile = edition
            .path()
            .resolve(&edition.config().identifier, tauri::path::BaseDirectory::LocalData)
            .unwrap();
        assert_eq!(webview_profile, edition.path().app_local_data_dir().unwrap());
    }

    #[cfg(feature = "microsoft-store")]
    #[test]
    fn store_starts_empty_and_session_writes_do_not_replace_direct_data() {
        let direct = app(tauri::generate_context!());
        let store = app(super::context());
        let root = tempfile::tempdir().unwrap();
        // Keep the actual Tauri-resolved directory suffixes while substituting
        // a temporary platform data root. Never touch a developer's live data.
        let data_root = direct.path().data_dir().unwrap();
        let direct_dir = root
            .path()
            .join(direct.path().app_data_dir().unwrap().strip_prefix(&data_root).unwrap());
        let store_dir = root
            .path()
            .join(store.path().app_data_dir().unwrap().strip_prefix(&data_root).unwrap());
        std::fs::create_dir_all(&direct_dir).unwrap();
        for filename in ["preferences.json", "last-session.canvaz", "last-project.txt"] {
            std::fs::write(direct_dir.join(filename), "direct data").unwrap();
            assert!(!store_dir.join(filename).exists());
        }

        let mut project = crate::project::Project::new();
        project.window_size_sec = Some(300);
        project.save(store_dir.join("last-session.canvaz").to_str().unwrap()).unwrap();
        std::fs::write(store_dir.join("preferences.json"), "{\"sidebarWidth\":440}").unwrap();
        std::fs::write(store_dir.join("last-project.txt"), "store-project.canvaz").unwrap();
        let restored = crate::project::Project::load(store_dir.join("last-session.canvaz").to_str().unwrap()).unwrap();
        assert_eq!(restored.window_size_sec, Some(300));
        for filename in ["preferences.json", "last-session.canvaz", "last-project.txt"] {
            assert_eq!(std::fs::read_to_string(direct_dir.join(filename)).unwrap(), "direct data");
        }
    }
}
