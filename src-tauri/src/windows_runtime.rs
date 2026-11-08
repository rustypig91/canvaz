//! Recover from a missing Evergreen runtime before creating any webview.

const DOWNLOAD_URL: &str = "https://developer.microsoft.com/en-us/microsoft-edge/webview2/#download-section";
const TITLE: &str = "Canvaz - WebView2 required";

pub fn prepare() -> bool {
    let available = match tauri::webview_version() {
        Ok(version) => !version.trim().is_empty(),
        Err(error) => {
            log::error!("Could not detect the WebView2 runtime: {error}");
            false
        }
    };
    recover(
        available,
        || {
            rfd::MessageDialog::new()
                .set_title(TITLE)
                .set_level(rfd::MessageLevel::Warning)
                .set_description(
                    "Canvaz needs the Microsoft Edge WebView2 Runtime to display its interface.\n\n\
                     Open Microsoft's download page? Install the Evergreen Runtime, then reopen Canvaz.\n\n\
                     For offline or managed computers, ask your administrator to install the runtime.",
                )
                .set_buttons(rfd::MessageButtons::YesNo)
                .show()
                == rfd::MessageDialogResult::Yes
        },
        || tauri_plugin_opener::open_url(DOWNLOAD_URL, None::<&str>).map_err(|error| error.to_string()),
        |error| {
            log::error!("Could not open WebView2 download page: {error}");
            rfd::MessageDialog::new()
                .set_title(TITLE)
                .set_level(rfd::MessageLevel::Error)
                .set_description(format!(
                    "Could not open your browser. Download and install the Evergreen WebView2 Runtime from:\n\n\
                     {DOWNLOAD_URL}\n\nThen reopen Canvaz."
                ))
                .show();
        },
    )
}

// Keep recovery independent of Windows dialogs so all decisions can be checked
// without changing the developer's installed runtime or opening a browser.
fn recover(
    available: bool,
    ask_to_download: impl FnOnce() -> bool,
    open_download: impl FnOnce() -> Result<(), String>,
    show_error: impl FnOnce(&str),
) -> bool {
    if available {
        return true;
    }
    if ask_to_download() {
        if let Err(error) = open_download() {
            show_error(&error);
        }
    }
    // Installing is a separate user action. Never create a webview until the
    // runtime is detected on a subsequent launch.
    false
}

#[cfg(test)]
mod tests {
    use super::recover;

    #[test]
    fn installed_runtime_starts_without_prompting_or_opening_a_browser() {
        assert!(recover(
            true,
            || panic!("unexpected prompt"),
            || panic!("unexpected download"),
            |_| panic!("unexpected error")
        ));
    }

    #[test]
    fn declining_download_exits_without_opening_a_browser() {
        assert!(!recover(
            false,
            || false,
            || panic!("unexpected download"),
            |_| panic!("unexpected error")
        ));
    }

    #[test]
    fn accepting_download_opens_the_page_and_exits_before_webview_creation() {
        let mut opened = false;
        assert!(!recover(
            false,
            || true,
            || {
                opened = true;
                Ok(())
            },
            |_| panic!("unexpected error")
        ));
        assert!(opened);
    }

    #[test]
    fn browser_failure_shows_recovery_instructions_and_exits() {
        let mut shown = None;
        assert!(!recover(
            false,
            || true,
            || Err("browser unavailable".into()),
            |error| shown = Some(error.to_owned())
        ));
        assert_eq!(shown.as_deref(), Some("browser unavailable"));
    }
}
