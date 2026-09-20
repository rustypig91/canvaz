//! A native app launched from a Snap IDE must use the host's GTK/GIO modules.
//! Snap modules can pull core20's libc into the host WebKit subprocesses.

use std::ffi::{OsStr, OsString};
use std::path::Path;

fn is_snap_mount(path: &Path) -> bool {
    path.starts_with("/snap") || path.starts_with("/var/lib/snapd/snap")
}

fn is_snap_path(path: &Path) -> bool {
    is_snap_mount(path)
        // Snap IDEs also keep GIO and input-method caches under ~/snap/.
        || path.starts_with("/root/snap")
        || (path.starts_with("/home") && path.components().nth(3).is_some_and(|p| p.as_os_str() == "snap"))
        || std::env::var_os("HOME").is_some_and(|home| path.starts_with(Path::new(&home).join("snap")))
}

fn host_paths(value: &OsStr) -> Option<OsString> {
    let paths: Vec<_> = std::env::split_paths(value).filter(|p| !is_snap_path(p)).collect();
    if paths.is_empty() {
        None
    } else {
        std::env::join_paths(paths).ok()
    }
}

/// Call once, before GTK, the logger, or any application threads are started.
pub fn prepare() {
    // A future actual Snap package needs its own runtime paths.
    if std::env::current_exe().is_ok_and(|p| is_snap_mount(&p)) {
        return;
    }
    for name in [
        "GTK_PATH", "GTK_EXE_PREFIX", "GTK_IM_MODULE_FILE",
        "GIO_MODULE_DIR", "GIO_EXTRA_MODULES", "GDK_PIXBUF_MODULE_FILE",
        "GDK_PIXBUF_MODULEDIR", "GSETTINGS_SCHEMA_DIR", "LD_LIBRARY_PATH",
    ] {
        if let Some(value) = std::env::var_os(name) {
            match host_paths(&value) {
                Some(cleaned) if cleaned != value => std::env::set_var(name, cleaned),
                None => std::env::remove_var(name),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_snap_modules_and_cached_module_locations() {
        for value in [
            "/snap/code/264/usr/lib/gtk-3.0",
            "/var/lib/snapd/snap/code/current/usr",
            "/home/user/snap/code/common/.cache/gio-modules",
        ] {
            assert_eq!(host_paths(OsStr::new(value)), None);
        }
    }

    #[test]
    fn preserves_native_and_appimage_paths() {
        let value = "/usr/local/lib:/tmp/.mount_Canvaz/usr/lib:/opt/snapshot/lib:/opt/vendor/snap/lib";
        assert_eq!(host_paths(OsStr::new(value)), Some(value.into()));
        assert_eq!(
            host_paths(OsStr::new("/snap/core20/current/lib:/usr/lib:/snap/code/current/lib")),
            Some("/usr/lib".into())
        );
    }
}
