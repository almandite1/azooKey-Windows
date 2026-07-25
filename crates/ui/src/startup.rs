//! Process-startup decisions: which pipe name to serve, and where WebView2
//! keeps its profile. Both take their input explicitly so they can be tested
//! without an environment or a command line.

/// The pipe base name to serve, taken from ui.exe's own command line
/// (`--pipe-base <name>` or `--pipe-base=<name>`). `None` means "use the
/// session's real name".
///
/// This exists for the display tests (`tests/window_display.rs`), which have
/// to stand up their own ui.exe alongside the installed IME. The listener asks
/// for the FIRST pipe instance, so a second process on the real name simply
/// fails to start.
///
/// Deliberately an argument and deliberately server-side only: an override the
/// *clients* honoured — an environment variable, say, which any process can
/// plant in the user's session — would redirect the TIP onto a pipe an
/// attacker owns, i.e. hand over the keystroke stream. This one cannot: all it
/// does is make the process that was launched with it serve a name nobody
/// talks to. The launcher never passes it.
pub fn pipe_base_from_args<I>(args: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let value = match arg.strip_prefix("--pipe-base=") {
            Some(inline) => Some(inline.to_string()),
            None if arg == "--pipe-base" => args.next(),
            None => continue,
        };
        // an empty name would build `\\.\pipe\` and fail obscurely later;
        // treat it as absent and keep the real name
        return value.filter(|name| !name.is_empty());
    }
    None
}

/// Where WebView2 keeps its profile, given the resolved per-user data root.
///
/// Left unset it defaults to `<exe dir>\ui.exe.WebView2`, and the exe dir is
/// Program Files: a standard (non-elevated) user cannot create it there. That
/// is not a soft failure — environment creation returns 0x80080005 and the
/// webview never builds, so neither window ever appears. The logon task's
/// `HighestAvailable` does not save us; it is a no-op for standard users
/// (issue #54).
///
/// Falls back to the temp dir rather than to wry's default: with no
/// `LOCALAPPDATA` we still need somewhere every user can write, and the
/// profile is a cache we are free to lose.
pub fn webview2_data_dir_in(local_data_root: Option<std::path::PathBuf>) -> std::path::PathBuf {
    match local_data_root {
        Some(root) => root.join("WebView2"),
        None => std::env::temp_dir().join("Azookey").join("WebView2"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pipe_base_argument_keeps_the_real_name() {
        assert_eq!(pipe_base_from_args([]), None);
        assert_eq!(
            pipe_base_from_args(["--something-else".to_string()]),
            None,
            "an unrelated argument must not be mistaken for the override"
        );
    }

    #[test]
    fn the_pipe_base_is_accepted_in_both_spellings() {
        assert_eq!(
            pipe_base_from_args(["--pipe-base".to_string(), "azookey_ui_test".to_string()]),
            Some("azookey_ui_test".to_string())
        );
        assert_eq!(
            pipe_base_from_args(["--pipe-base=azookey_ui_test".to_string()]),
            Some("azookey_ui_test".to_string())
        );
    }

    /// `--pipe-base` with nothing after it (or with an empty name) would build
    /// `\\.\pipe\` and fail deep inside the listener; fall back to the real
    /// name instead.
    #[test]
    fn an_empty_pipe_base_falls_back_to_the_real_name() {
        assert_eq!(pipe_base_from_args(["--pipe-base".to_string()]), None);
        assert_eq!(pipe_base_from_args(["--pipe-base=".to_string()]), None);
    }

    /// A standard user cannot write next to ui.exe, so the profile goes under
    /// the per-user data root when there is one (issue #54).
    #[test]
    fn the_webview_profile_lives_under_the_user_data_root() {
        let root = std::path::PathBuf::from(r"C:\Users\someone\AppData\Local\Azookey");

        assert_eq!(
            webview2_data_dir_in(Some(root.clone())),
            root.join("WebView2")
        );
    }

    /// With no `LOCALAPPDATA` the profile is a cache we are free to lose, so
    /// it goes somewhere every user can write rather than back to wry's
    /// (unwritable) default.
    #[test]
    fn the_webview_profile_falls_back_to_the_temp_directory() {
        assert_eq!(
            webview2_data_dir_in(None),
            std::env::temp_dir().join("Azookey").join("WebView2")
        );
    }
}
