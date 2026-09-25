//! Bounded discovery of native Codex executables without invoking package-manager shims.

use std::{
    cmp::Reverse,
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
    time::SystemTime,
};

use sylvops_core::provider::provider_error;

const MAX_PATH_DIRECTORIES: usize = 128;
const MAX_DESKTOP_CACHE_ENTRIES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeFormat {
    Elf,
    MachO,
    Pe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CodexTarget {
    triple: &'static str,
    npm_package: &'static str,
    executable_name: &'static str,
    format: NativeFormat,
}

#[derive(Debug)]
struct DiscoveryRoots {
    explicit: Option<PathBuf>,
    path_directories: Vec<PathBuf>,
    direct_candidates: Vec<PathBuf>,
    desktop_cache_roots: Vec<PathBuf>,
}

pub(crate) fn discover_codex_executable() -> sylvops_core::Result<PathBuf> {
    let target = current_target().ok_or_else(|| {
        provider_error("Codex discovery does not support this operating system or architecture")
    })?;
    discover_with_roots(target, &DiscoveryRoots::from_process(target)).ok_or_else(|| {
        provider_error(
            "Codex was not found as a native executable in CODEX_CLI_PATH, PATH, or a supported official install location",
        )
    })
}

impl DiscoveryRoots {
    fn from_process(target: CodexTarget) -> Self {
        let explicit = std::env::var_os("CODEX_CLI_PATH")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        let path_directories = std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .filter(|directory| directory.is_absolute())
                    .take(MAX_PATH_DIRECTORIES)
                    .collect()
            })
            .unwrap_or_default();
        let mut direct_candidates = Vec::new();
        let mut desktop_cache_roots = Vec::new();

        if let Some(home) = platform_home() {
            let standalone = home
                .join(".codex")
                .join("packages")
                .join("standalone")
                .join("current");
            direct_candidates.push(standalone.join(target.executable_name));
            direct_candidates.push(standalone.join("bin").join(target.executable_name));
        }

        #[cfg(windows)]
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            direct_candidates.push(
                local_app_data
                    .join("Programs")
                    .join("OpenAI")
                    .join("Codex")
                    .join("bin")
                    .join(target.executable_name),
            );
            desktop_cache_roots.push(local_app_data.join("OpenAI").join("Codex").join("bin"));
            desktop_cache_roots.push(
                local_app_data
                    .join("Packages")
                    .join("OpenAI.Codex_2p2nqsd0c76g0")
                    .join("LocalCache")
                    .join("Local")
                    .join("OpenAI")
                    .join("Codex")
                    .join("bin"),
            );
        }

        #[cfg(target_os = "macos")]
        {
            for applications in [
                PathBuf::from("/Applications"),
                platform_home().unwrap_or_default().join("Applications"),
            ] {
                for bundle in ["ChatGPT.app", "Codex.app"] {
                    direct_candidates.push(
                        applications
                            .join(bundle)
                            .join("Contents")
                            .join("Resources")
                            .join(target.executable_name),
                    );
                }
            }
        }

        Self {
            explicit,
            path_directories,
            direct_candidates,
            desktop_cache_roots,
        }
    }
}

fn discover_with_roots(target: CodexTarget, roots: &DiscoveryRoots) -> Option<PathBuf> {
    if let Some(path) = roots
        .explicit
        .as_deref()
        .and_then(|path| native_path(path, target.format))
    {
        return Some(path);
    }

    for directory in &roots.path_directories {
        if let Some(path) = native_path(&directory.join(target.executable_name), target.format) {
            return Some(path);
        }
        if let Some(path) = npm_candidate(
            &directory.join("node_modules").join("@openai").join("codex"),
            target,
        ) {
            return Some(path);
        }
        if let Some(package_root) = npm_package_root_from_wrapper(directory, target)
            && let Some(path) = npm_candidate(&package_root, target)
        {
            return Some(path);
        }
    }

    for candidate in &roots.direct_candidates {
        if let Some(path) = native_path(candidate, target.format) {
            return Some(path);
        }
    }
    for cache_root in &roots.desktop_cache_roots {
        if let Some(path) = desktop_cache_candidate(cache_root, target) {
            return Some(path);
        }
    }
    None
}

fn npm_package_root_from_wrapper(directory: &Path, target: CodexTarget) -> Option<PathBuf> {
    let wrapper = std::fs::canonicalize(directory.join(if target.format == NativeFormat::Pe {
        "codex"
    } else {
        target.executable_name
    }))
    .ok()?;
    let bin = wrapper.parent()?;
    if bin.file_name()? != "bin" || wrapper.file_name()? != "codex.js" {
        return None;
    }
    let package_root = bin.parent()?;
    (package_root.file_name()? == "codex" && package_root.parent()?.file_name()? == "@openai")
        .then(|| package_root.to_path_buf())
}

fn npm_candidate(package_root: &Path, target: CodexTarget) -> Option<PathBuf> {
    let suffix = Path::new("vendor")
        .join(target.triple)
        .join("bin")
        .join(target.executable_name);
    for candidate in [
        package_root
            .join("node_modules")
            .join(target.npm_package)
            .join(&suffix),
        package_root.join(&suffix),
    ] {
        if let Some(path) = native_path(&candidate, target.format) {
            return Some(path);
        }
    }
    None
}

fn desktop_cache_candidate(root: &Path, target: CodexTarget) -> Option<PathBuf> {
    if let Some(path) = native_path(&root.join(target.executable_name), target.format) {
        return Some(path);
    }
    let entries = std::fs::read_dir(root).ok()?;
    let mut candidates = entries
        .take(MAX_DESKTOP_CACHE_ENTRIES)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            let name = entry.file_name();
            let name = name.to_str()?;
            if !file_type.is_dir()
                || file_type.is_symlink()
                || name.len() != 16
                || !name.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return None;
            }
            let candidate = entry.path().join(target.executable_name);
            let modified = candidate
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((Reverse(modified), candidate))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(modified, _)| *modified);
    candidates
        .into_iter()
        .find_map(|(_, candidate)| native_path(&candidate, target.format))
}

fn native_path(candidate: &Path, format: NativeFormat) -> Option<PathBuf> {
    if !native_executable(candidate, format) {
        return None;
    }
    let canonical = std::fs::canonicalize(candidate).ok()?;
    native_executable(&canonical, format).then_some(canonical)
}

fn native_executable(path: &Path, format: NativeFormat) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    let mut header = [0_u8; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok()
        && format.matches(header)
}

impl NativeFormat {
    fn matches(self, header: [u8; 4]) -> bool {
        match self {
            Self::Elf => header == [0x7f, b'E', b'L', b'F'],
            Self::Pe => header[..2] == *b"MZ",
            Self::MachO => matches!(
                header,
                [0xfe, 0xed, 0xfa, 0xce | 0xcf]
                    | [0xce | 0xcf, 0xfa, 0xed, 0xfe]
                    | [0xca, 0xfe, 0xba, 0xbe | 0xbf]
                    | [0xbe | 0xbf, 0xba, 0xfe, 0xca]
            ),
        }
    }
}

#[cfg(windows)]
fn platform_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(unix)]
fn platform_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn current_target() -> Option<CodexTarget> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(WINDOWS_X64)
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        Some(WINDOWS_ARM64)
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(LINUX_X64)
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(LINUX_ARM64)
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some(MACOS_X64)
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(MACOS_ARM64)
    } else {
        None
    }
}

const WINDOWS_X64: CodexTarget = CodexTarget {
    triple: "x86_64-pc-windows-msvc",
    npm_package: "@openai/codex-win32-x64",
    executable_name: "codex.exe",
    format: NativeFormat::Pe,
};
const WINDOWS_ARM64: CodexTarget = CodexTarget {
    triple: "aarch64-pc-windows-msvc",
    npm_package: "@openai/codex-win32-arm64",
    executable_name: "codex.exe",
    format: NativeFormat::Pe,
};
const LINUX_X64: CodexTarget = CodexTarget {
    triple: "x86_64-unknown-linux-musl",
    npm_package: "@openai/codex-linux-x64",
    executable_name: "codex",
    format: NativeFormat::Elf,
};
const LINUX_ARM64: CodexTarget = CodexTarget {
    triple: "aarch64-unknown-linux-musl",
    npm_package: "@openai/codex-linux-arm64",
    executable_name: "codex",
    format: NativeFormat::Elf,
};
const MACOS_X64: CodexTarget = CodexTarget {
    triple: "x86_64-apple-darwin",
    npm_package: "@openai/codex-darwin-x64",
    executable_name: "codex",
    format: NativeFormat::MachO,
};
const MACOS_ARM64: CodexTarget = CodexTarget {
    triple: "aarch64-apple-darwin",
    npm_package: "@openai/codex-darwin-arm64",
    executable_name: "codex",
    format: NativeFormat::MachO,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_npm_layout_resolves_native_binary_without_using_shims() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let bin = temporary.path().join("npm");
        std::fs::create_dir_all(&bin).expect("npm bin");
        std::fs::write(bin.join("codex.cmd"), "@node codex.js\r\n").expect("cmd shim");
        std::fs::write(bin.join("codex.ps1"), "node codex.js\n").expect("PowerShell shim");
        let native = npm_fixture(&bin, WINDOWS_X64);

        let discovered = discover_with_roots(
            WINDOWS_X64,
            &DiscoveryRoots {
                explicit: None,
                path_directories: vec![bin],
                direct_candidates: Vec::new(),
                desktop_cache_roots: Vec::new(),
            },
        )
        .expect("native npm Codex");

        assert_eq!(discovered, std::fs::canonicalize(native).unwrap());
    }

    #[test]
    fn npm_layouts_are_supported_for_every_beta_target() {
        for target in [
            WINDOWS_X64,
            WINDOWS_ARM64,
            LINUX_X64,
            LINUX_ARM64,
            MACOS_X64,
            MACOS_ARM64,
        ] {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let bin = temporary.path().join("bin");
            let native = npm_fixture(&bin, target);
            let discovered = discover_with_roots(
                target,
                &DiscoveryRoots {
                    explicit: None,
                    path_directories: vec![bin],
                    direct_candidates: Vec::new(),
                    desktop_cache_roots: Vec::new(),
                },
            )
            .expect("native npm Codex");
            assert_eq!(discovered, std::fs::canonicalize(native).unwrap());
        }
    }

    #[test]
    fn desktop_cache_uses_newest_bounded_native_candidate() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path().join("OpenAI").join("Codex").join("bin");
        let older = root.join("0123456789abcdef").join("codex.exe");
        write_native(&older, NativeFormat::Pe);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = root.join("fedcba9876543210").join("codex.exe");
        write_native(&newer, NativeFormat::Pe);
        std::fs::create_dir_all(root.join("not-an-install")).unwrap();

        let discovered = desktop_cache_candidate(&root, WINDOWS_X64).expect("desktop Codex");
        assert_eq!(discovered, std::fs::canonicalize(newer).unwrap());
    }

    #[test]
    fn scripts_and_wrong_native_formats_are_rejected() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let script = temporary.path().join("codex.exe");
        std::fs::write(&script, "#!/usr/bin/env node\n").expect("script");
        make_executable(&script);
        assert!(native_path(&script, NativeFormat::Pe).is_none());

        let elf = temporary.path().join("native.exe");
        write_native(&elf, NativeFormat::Elf);
        assert!(native_path(&elf, NativeFormat::Pe).is_none());
    }

    #[test]
    fn explicit_native_path_precedes_path_and_known_locations() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let explicit = temporary.path().join("explicit").join("codex.exe");
        let on_path = temporary.path().join("path").join("codex.exe");
        write_native(&explicit, NativeFormat::Pe);
        write_native(&on_path, NativeFormat::Pe);
        let discovered = discover_with_roots(
            WINDOWS_X64,
            &DiscoveryRoots {
                explicit: Some(explicit.clone()),
                path_directories: vec![on_path.parent().unwrap().to_path_buf()],
                direct_candidates: Vec::new(),
                desktop_cache_roots: Vec::new(),
            },
        )
        .expect("explicit Codex");
        assert_eq!(discovered, std::fs::canonicalize(explicit).unwrap());
    }

    fn npm_fixture(bin: &Path, target: CodexTarget) -> PathBuf {
        let native = bin
            .join("node_modules")
            .join("@openai")
            .join("codex")
            .join("node_modules")
            .join(target.npm_package)
            .join("vendor")
            .join(target.triple)
            .join("bin")
            .join(target.executable_name);
        write_native(&native, target.format);
        native
    }

    fn write_native(path: &Path, format: NativeFormat) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("native parent");
        let header: &[u8] = match format {
            NativeFormat::Elf => b"\x7fELFfixture",
            NativeFormat::MachO => b"\xfe\xed\xfa\xcffixture",
            NativeFormat::Pe => b"MZfixture",
        };
        std::fs::write(path, header).expect("native fixture");
        make_executable(path);
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(windows)]
    fn make_executable(_path: &Path) {}
}
