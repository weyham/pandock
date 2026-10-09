//! Runtime data directory resolution.
//!
//! Priority order:
//! 1. `--data-dir <path>` command line argument
//! 2. `PANDOCK_DATA_DIR` environment variable
//! 3. Portable mode: a `data\` directory next to the executable
//! 4. Fixed per-user directory: `%APPDATA%\Pandock`
//!
//! Velopack packages ship without an exe-adjacent `data\` folder, so
//! installed instances land on rule 4, while portable zip builds ship
//! with an empty `data\` folder and stay on rule 3.

use std::path::{Path, PathBuf};

const PORTABLE_DIR_NAME: &str = "data";
const FIXED_DIR_NAME: &str = "Pandock";
const ENV_DATA_DIR: &str = "PANDOCK_DATA_DIR";
const ARG_DATA_DIR: &str = "--data-dir";

/// Pure resolution logic, separated from process state for testability.
pub fn select_data_dir(
    cli: Option<PathBuf>,
    env: Option<PathBuf>,
    exe_dir: &Path,
    roaming_dir: &Path,
    portable_exists: bool,
) -> PathBuf {
    if let Some(dir) = cli {
        return dir;
    }
    if let Some(dir) = env {
        return dir;
    }
    if portable_exists {
        return exe_dir.join(PORTABLE_DIR_NAME);
    }
    roaming_dir.join(FIXED_DIR_NAME)
}

fn cli_data_dir<I>(args: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == ARG_DATA_DIR {
            if let Some(value) = iter.next() {
                if !value.trim().is_empty() {
                    return Some(PathBuf::from(value));
                }
            }
        } else if let Some(value) = arg.strip_prefix("--data-dir=") {
            if !value.trim().is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

fn env_data_dir() -> Option<PathBuf> {
    std::env::var(ENV_DATA_DIR)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

/// Resolve the runtime data directory for the current process.
pub fn resolve() -> Result<PathBuf, String> {
    let exe_dir = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or_else(|| "无法定位应用目录".to_string())?
        .to_path_buf();
    let roaming = std::env::var("APPDATA")
        .map(PathBuf::from)
        .map_err(|_| "无法定位 %APPDATA% 目录".to_string())?;
    let portable_exists = exe_dir.join(PORTABLE_DIR_NAME).is_dir();
    let dir = select_data_dir(
        cli_data_dir(std::env::args().skip(1)),
        env_data_dir(),
        &exe_dir,
        &roaming,
        portable_exists,
    );
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe_dir() -> PathBuf {
        PathBuf::from(r"C:\Apps\Pandock")
    }

    fn roaming() -> PathBuf {
        PathBuf::from(r"C:\Users\test\AppData\Roaming")
    }

    #[test]
    fn cli_argument_wins_over_everything() {
        let dir = select_data_dir(
            Some(PathBuf::from(r"D:\custom")),
            Some(PathBuf::from(r"E:\env")),
            &exe_dir(),
            &roaming(),
            true,
        );
        assert_eq!(dir, PathBuf::from(r"D:\custom"));
    }

    #[test]
    fn env_wins_over_portable_and_fixed() {
        let dir = select_data_dir(
            None,
            Some(PathBuf::from(r"E:\env")),
            &exe_dir(),
            &roaming(),
            true,
        );
        assert_eq!(dir, PathBuf::from(r"E:\env"));
    }

    #[test]
    fn portable_data_dir_next_to_exe_wins_over_fixed() {
        let dir = select_data_dir(None, None, &exe_dir(), &roaming(), true);
        assert_eq!(dir, exe_dir().join("data"));
    }

    #[test]
    fn falls_back_to_fixed_roaming_dir() {
        let dir = select_data_dir(None, None, &exe_dir(), &roaming(), false);
        assert_eq!(dir, roaming().join("Pandock"));
    }

    #[test]
    fn parses_data_dir_argument_forms() {
        assert_eq!(
            cli_data_dir(vec!["--data-dir".into(), r"F:\data".into()]),
            Some(PathBuf::from(r"F:\data"))
        );
        assert_eq!(
            cli_data_dir(vec!["--data-dir=F:\\data".into()]),
            Some(PathBuf::from(r"F:\data"))
        );
        assert_eq!(cli_data_dir(vec!["--autostart".into()]), None);
        // Velopack lifecycle args must be ignored.
        assert_eq!(
            cli_data_dir(vec![
                "--veloapp-install".into(),
                "--data-dir".into(),
                r"F:\data".into(),
            ]),
            Some(PathBuf::from(r"F:\data"))
        );
        assert_eq!(cli_data_dir(vec!["--data-dir".into(), "  ".into()]), None);
    }
}
