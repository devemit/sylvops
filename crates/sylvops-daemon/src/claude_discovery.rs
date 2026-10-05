//! Bounded discovery of native Claude Code executables.

use std::{
    fs::File,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
};

use sylvops_core::provider::provider_error;

const MAX_PATH_DIRECTORIES: usize = 128;
#[cfg(windows)]
const MAX_KNOWN_LAYOUT_ENTRIES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeFormat {
    Elf,
    MachO,
    Pe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Architecture {
    X86_64,
    Arm64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClaudeTarget {
    format: NativeFormat,
    architecture: Architecture,
    npm_package: &'static str,
    executable_name: &'static str,
}

#[derive(Debug)]
struct DiscoveryRoots {
    explicit: Option<PathBuf>,
    path_directories: Vec<PathBuf>,
    native_candidates: Vec<KnownCandidate>,
    package_manager_candidates: Vec<KnownCandidate>,
}

#[derive(Debug)]
struct KnownCandidate {
    path: PathBuf,
    redirect_root: Option<PathBuf>,
}

pub(crate) fn discover_claude_executable() -> sylvops_core::Result<PathBuf> {
    let target = current_target().ok_or_else(|| {
        provider_error(
            "Claude Code discovery does not support this operating system or architecture",
        )
    })?;
    discover_with_roots(target, &DiscoveryRoots::from_process(target)).ok_or_else(|| {
        provider_error(
            "Claude Code was not found as a native executable in CLAUDE_CLI_PATH, PATH, or a supported official install location",
        )
    })
}

pub(crate) fn revalidate_claude_executable(path: &Path) -> sylvops_core::Result<PathBuf> {
    let target = current_target().ok_or_else(|| {
        provider_error(
            "Claude Code discovery does not support this operating system or architecture",
        )
    })?;
    native_path(path, target)
        .filter(|canonical| canonical == path)
        .ok_or_else(|| provider_error("Claude Code executable identity changed after discovery"))
}

impl DiscoveryRoots {
    fn from_process(target: ClaudeTarget) -> Self {
        let explicit = std::env::var_os("CLAUDE_CLI_PATH")
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
        let mut native_candidates = Vec::new();
        let mut package_manager_candidates = Vec::new();
        if let Some(home) = platform_home() {
            #[cfg(unix)]
            let redirect_root = Some(
                home.join(".local")
                    .join("share")
                    .join("claude")
                    .join("versions"),
            );
            #[cfg(windows)]
            let redirect_root = None;
            native_candidates.push(KnownCandidate {
                path: home.join(".local").join("bin").join(target.executable_name),
                redirect_root,
            });
        }
        #[cfg(target_os = "linux")]
        for root in [PathBuf::from("/usr"), PathBuf::from("/usr/local")] {
            package_manager_candidates.push(KnownCandidate {
                path: root.join("bin").join(target.executable_name),
                redirect_root: Some(root),
            });
        }
        #[cfg(target_os = "macos")]
        for root in [PathBuf::from("/opt/homebrew"), PathBuf::from("/usr/local")] {
            package_manager_candidates.push(KnownCandidate {
                path: root.join("bin").join(target.executable_name),
                redirect_root: Some(root),
            });
        }
        #[cfg(windows)]
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            let winget = local_app_data.join("Microsoft").join("WinGet");
            package_manager_candidates.push(KnownCandidate {
                path: winget.join("Links").join(target.executable_name),
                redirect_root: Some(winget.clone()),
            });
            let packages = winget.join("Packages");
            if let Ok(entries) = std::fs::read_dir(&packages) {
                package_manager_candidates.extend(
                    entries
                        .take(MAX_KNOWN_LAYOUT_ENTRIES)
                        .filter_map(Result::ok)
                        .filter(|entry| {
                            entry
                                .file_name()
                                .to_string_lossy()
                                .starts_with("Anthropic.ClaudeCode_")
                        })
                        .map(|entry| KnownCandidate {
                            path: entry.path().join(target.executable_name),
                            redirect_root: Some(packages.clone()),
                        }),
                );
            }
        }
        Self {
            explicit,
            path_directories,
            native_candidates,
            package_manager_candidates,
        }
    }
}

fn discover_with_roots(target: ClaudeTarget, roots: &DiscoveryRoots) -> Option<PathBuf> {
    if let Some(path) = roots
        .explicit
        .as_deref()
        .and_then(|path| native_path(path, target))
    {
        return Some(path);
    }
    for directory in &roots.path_directories {
        if let Some(path) = native_path(&directory.join(target.executable_name), target) {
            return Some(path);
        }
    }
    for candidate in &roots.native_candidates {
        if let Some(path) = known_native_path(candidate, target) {
            return Some(path);
        }
    }
    for candidate in &roots.package_manager_candidates {
        if let Some(path) = known_native_path(candidate, target) {
            return Some(path);
        }
    }
    roots
        .path_directories
        .iter()
        .find_map(|directory| npm_candidate(directory, target))
}

fn native_path(candidate: &Path, target: ClaudeTarget) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(candidate).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    let canonical = std::fs::canonicalize(candidate).ok()?;
    if unsupported_host_path(&canonical) {
        return None;
    }
    revalidate_opened_path(candidate, canonical, target)
}

fn known_native_path(candidate: &KnownCandidate, target: ClaudeTarget) -> Option<PathBuf> {
    if let Some(root) = candidate.redirect_root.as_deref() {
        let root = std::fs::canonicalize(root).ok()?;
        let canonical = std::fs::canonicalize(&candidate.path).ok()?;
        if !canonical.starts_with(&root) || unsupported_host_path(&canonical) {
            return None;
        }
    } else {
        return native_path(&candidate.path, target);
    }
    let canonical = std::fs::canonicalize(&candidate.path).ok()?;
    revalidate_opened_path(&candidate.path, canonical, target)
}

fn revalidate_opened_path(
    candidate: &Path,
    canonical: PathBuf,
    target: ClaudeTarget,
) -> Option<PathBuf> {
    let mut file = File::open(&canonical).ok()?;
    let opened = same_file::Handle::from_file(file.try_clone().ok()?).ok()?;
    if !native_identity_matches(&mut file, target)
        || std::fs::canonicalize(candidate).ok()? != canonical
        || same_file::Handle::from_path(candidate).ok()? != opened
    {
        return None;
    }
    Some(canonical)
}

fn npm_candidate(directory: &Path, target: ClaudeTarget) -> Option<PathBuf> {
    let package_roots = [
        directory
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code"),
        directory
            .parent()?
            .join("lib")
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code"),
    ];
    for package_root in package_roots {
        let candidates = [
            package_root.join("bin").join("claude.exe"),
            package_root
                .join("node_modules")
                .join(target.npm_package)
                .join(target.executable_name),
        ];
        for candidate in candidates {
            let Ok(root) = std::fs::canonicalize(&package_root) else {
                continue;
            };
            if let Some(path) = native_path(&candidate, target)
                && path.starts_with(&root)
            {
                return Some(path);
            }
        }
    }
    None
}

fn unsupported_host_path(path: &Path) -> bool {
    let path = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    path.contains("/windowsapps/")
        || path.contains("/shims/")
        || path.contains("/chocolatey/bin/")
        || path.contains("/choco/bin/")
        || path.contains("/anthropicclaude/")
        || path.contains("/programs/claude-desktop/")
        || path.contains("/claude/claude-code/")
        || path.contains("/claude.app/")
        || path.contains("/claude desktop/")
        || path.contains("/wsl/")
        || path.contains("/wsl.localhost/")
        || path.starts_with("//wsl")
        || path.contains("/lxss/")
}

fn native_identity_matches(file: &mut File, target: ClaudeTarget) -> bool {
    match target.format {
        NativeFormat::Elf => {
            let mut header = [0_u8; 20];
            if file.read_exact(&mut header).is_err()
                || header[..4] != *b"\x7fELF"
                || header[4] != 2
                || header[5] != 1
            {
                return false;
            }
            let machine = u16::from_le_bytes([header[18], header[19]]);
            machine
                == match target.architecture {
                    Architecture::X86_64 => 0x3e,
                    Architecture::Arm64 => 0xb7,
                }
        }
        NativeFormat::Pe => {
            let mut dos = [0_u8; 64];
            if file.read_exact(&mut dos).is_err() || dos[..2] != *b"MZ" {
                return false;
            }
            let offset = u64::from(u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap()));
            if offset > 1024 * 1024 || file.seek(SeekFrom::Start(offset)).is_err() {
                return false;
            }
            let mut coff = [0_u8; 6];
            if file.read_exact(&mut coff).is_err() || coff[..4] != *b"PE\0\0" {
                return false;
            }
            let machine = u16::from_le_bytes([coff[4], coff[5]]);
            machine
                == match target.architecture {
                    Architecture::X86_64 => 0x8664,
                    Architecture::Arm64 => 0xaa64,
                }
        }
        NativeFormat::MachO => {
            let mut header = [0_u8; 8];
            if file.read_exact(&mut header).is_err() {
                return false;
            }
            let expected = match target.architecture {
                Architecture::X86_64 => 0x0100_0007,
                Architecture::Arm64 => 0x0100_000c,
            };
            match header[..4] {
                [0xfe, 0xed, 0xfa, 0xcf] => {
                    u32::from_be_bytes(header[4..8].try_into().unwrap()) == expected
                }
                [0xcf, 0xfa, 0xed, 0xfe] => {
                    u32::from_le_bytes(header[4..8].try_into().unwrap()) == expected
                }
                [0xca, 0xfe, 0xba, 0xbe | 0xbf] => {
                    fat_mach_o_contains(file, header, expected, true)
                }
                [0xbe | 0xbf, 0xba, 0xfe, 0xca] => {
                    fat_mach_o_contains(file, header, expected, false)
                }
                _ => false,
            }
        }
    }
}

fn fat_mach_o_contains(file: &mut File, header: [u8; 8], expected: u32, big_endian: bool) -> bool {
    let count_bytes: [u8; 4] = header[4..8].try_into().unwrap();
    let count = if big_endian {
        u32::from_be_bytes(count_bytes)
    } else {
        u32::from_le_bytes(count_bytes)
    };
    if count == 0 || count > 64 {
        return false;
    }
    let fat64 = matches!(
        header[..4],
        [0xca, 0xfe, 0xba, 0xbf] | [0xbf, 0xba, 0xfe, 0xca]
    );
    let entry_size = if fat64 { 32 } else { 20 };
    let mut entry = [0_u8; 32];
    for _ in 0..count {
        if file.read_exact(&mut entry[..entry_size]).is_err() {
            return false;
        }
        let cpu_bytes: [u8; 4] = entry[..4].try_into().unwrap();
        let cpu = if big_endian {
            u32::from_be_bytes(cpu_bytes)
        } else {
            u32::from_le_bytes(cpu_bytes)
        };
        if cpu == expected {
            return true;
        }
    }
    false
}

#[cfg(windows)]
fn platform_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(unix)]
fn platform_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn current_target() -> Option<ClaudeTarget> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(WINDOWS_X64)
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(LINUX_X64)
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some(MACOS_X64)
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(MACOS_ARM64)
    } else {
        None
    }
}

const WINDOWS_X64: ClaudeTarget = ClaudeTarget {
    format: NativeFormat::Pe,
    architecture: Architecture::X86_64,
    npm_package: "@anthropic-ai/claude-code-win32-x64",
    executable_name: "claude.exe",
};
const LINUX_X64: ClaudeTarget = ClaudeTarget {
    format: NativeFormat::Elf,
    architecture: Architecture::X86_64,
    npm_package: "@anthropic-ai/claude-code-linux-x64",
    executable_name: "claude",
};
const MACOS_X64: ClaudeTarget = ClaudeTarget {
    format: NativeFormat::MachO,
    architecture: Architecture::X86_64,
    npm_package: "@anthropic-ai/claude-code-darwin-x64",
    executable_name: "claude",
};
const MACOS_ARM64: ClaudeTarget = ClaudeTarget {
    format: NativeFormat::MachO,
    architecture: Architecture::Arm64,
    npm_package: "@anthropic-ai/claude-code-darwin-arm64",
    executable_name: "claude",
};

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn explicit_native_path_precedes_path_and_known_locations() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let explicit = temporary.path().join("explicit").join("claude.exe");
        let on_path = temporary.path().join("path").join("claude.exe");
        write_native(&explicit, WINDOWS_X64);
        write_native(&on_path, WINDOWS_X64);

        let discovered = discover_with_roots(
            WINDOWS_X64,
            &DiscoveryRoots {
                explicit: Some(explicit.clone()),
                path_directories: vec![on_path.parent().unwrap().to_path_buf()],
                native_candidates: Vec::new(),
                package_manager_candidates: Vec::new(),
            },
        )
        .expect("explicit Claude Code");

        assert_eq!(discovered, std::fs::canonicalize(explicit).unwrap());
    }

    #[test]
    fn native_installer_precedes_package_manager_and_npm_layouts() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let native = temporary.path().join("native").join("claude.exe");
        let package_manager = temporary.path().join("manager").join("claude.exe");
        let npm_root = temporary
            .path()
            .join("npm")
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code");
        let npm = npm_root
            .join("node_modules")
            .join(WINDOWS_X64.npm_package)
            .join("claude.exe");
        write_native(&native, WINDOWS_X64);
        write_native(&package_manager, WINDOWS_X64);
        write_native(&npm, WINDOWS_X64);

        let discovered = discover_with_roots(
            WINDOWS_X64,
            &DiscoveryRoots {
                explicit: None,
                path_directories: vec![temporary.path().join("npm")],
                native_candidates: vec![KnownCandidate {
                    path: native.clone(),
                    redirect_root: None,
                }],
                package_manager_candidates: vec![KnownCandidate {
                    path: package_manager,
                    redirect_root: None,
                }],
            },
        )
        .expect("native Claude Code");

        assert_eq!(discovered, std::fs::canonicalize(native).unwrap());
    }

    #[test]
    fn known_layout_rejects_a_directory_link_escape() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let allowed = temporary.path().join("allowed");
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let native = outside.join("claude.exe");
        write_native(&native, WINDOWS_X64);
        let link = allowed.join("escaped");
        create_directory_link(&outside, &link);

        assert!(
            known_native_path(
                &KnownCandidate {
                    path: link.join("claude.exe"),
                    redirect_root: Some(allowed),
                },
                WINDOWS_X64,
            )
            .is_none()
        );
    }

    #[test]
    fn macos_universal_binary_accepts_the_matching_platform_slice() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let universal = temporary.path().join("claude");
        let mut file = File::create(&universal).unwrap();
        file.write_all(&0xcafe_babe_u32.to_be_bytes()).unwrap();
        file.write_all(&2_u32.to_be_bytes()).unwrap();
        for cpu in [0x0100_0007_u32, 0x0100_000c_u32] {
            file.write_all(&cpu.to_be_bytes()).unwrap();
            file.write_all(&[0_u8; 16]).unwrap();
        }
        drop(file);
        #[cfg(unix)]
        make_executable(&universal);

        assert_eq!(
            native_path(&universal, MACOS_ARM64),
            Some(std::fs::canonicalize(universal).unwrap())
        );
    }

    #[test]
    fn npm_native_layouts_are_supported_for_every_required_target() {
        for target in [WINDOWS_X64, LINUX_X64, MACOS_X64, MACOS_ARM64] {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let bin = temporary.path().join("bin");
            let native = bin
                .join("node_modules")
                .join("@anthropic-ai")
                .join("claude-code")
                .join("node_modules")
                .join(target.npm_package)
                .join(target.executable_name);
            write_native(&native, target);

            assert_eq!(
                npm_candidate(&bin, target),
                Some(std::fs::canonicalize(native).unwrap())
            );
        }
    }

    #[test]
    fn unix_global_npm_prefix_is_checked_after_a_missing_bin_local_package() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let bin = temporary.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let native = temporary
            .path()
            .join("lib")
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code")
            .join("node_modules")
            .join(LINUX_X64.npm_package)
            .join(LINUX_X64.executable_name);
        write_native(&native, LINUX_X64);

        assert_eq!(
            npm_candidate(&bin, LINUX_X64),
            Some(std::fs::canonicalize(native).unwrap())
        );
    }

    #[test]
    fn scripts_wrong_architectures_and_desktop_locations_are_rejected() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let script = temporary.path().join("claude.exe");
        std::fs::write(&script, "#!/usr/bin/env node\n").unwrap();
        #[cfg(unix)]
        make_executable(&script);
        assert!(native_path(&script, WINDOWS_X64).is_none());

        let wrong_architecture = temporary.path().join("wrong-arch.exe");
        let windows_arm64 = ClaudeTarget {
            architecture: Architecture::Arm64,
            ..WINDOWS_X64
        };
        write_native(&wrong_architecture, windows_arm64);
        assert!(native_path(&wrong_architecture, WINDOWS_X64).is_none());

        for desktop_or_bridge in [
            temporary.path().join("WindowsApps").join("claude.exe"),
            temporary.path().join("AnthropicClaude").join("claude.exe"),
            temporary
                .path()
                .join("Claude.app")
                .join("Contents")
                .join("MacOS")
                .join("claude.exe"),
            temporary
                .path()
                .join("wsl.localhost")
                .join("Ubuntu")
                .join("claude.exe"),
            temporary
                .path()
                .join("scoop")
                .join("shims")
                .join("claude.exe"),
            temporary
                .path()
                .join("chocolatey")
                .join("bin")
                .join("claude.exe"),
        ] {
            write_native(&desktop_or_bridge, WINDOWS_X64);
            assert!(native_path(&desktop_or_bridge, WINDOWS_X64).is_none());
        }
    }

    fn write_native(path: &Path, target: ClaudeTarget) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("native parent");
        let mut file = File::create(path).expect("native fixture");
        match target.format {
            NativeFormat::Pe => {
                file.write_all(b"MZ").unwrap();
                file.seek(SeekFrom::Start(0x3c)).unwrap();
                file.write_all(&0x80_u32.to_le_bytes()).unwrap();
                file.seek(SeekFrom::Start(0x80)).unwrap();
                file.write_all(b"PE\0\0").unwrap();
                let machine = match target.architecture {
                    Architecture::X86_64 => 0x8664_u16,
                    Architecture::Arm64 => 0xaa64_u16,
                };
                file.write_all(&machine.to_le_bytes()).unwrap();
            }
            NativeFormat::Elf => {
                let mut header = [0_u8; 20];
                header[..4].copy_from_slice(b"\x7fELF");
                header[4] = 2;
                header[5] = 1;
                let machine = match target.architecture {
                    Architecture::X86_64 => 0x3e_u16,
                    Architecture::Arm64 => 0xb7_u16,
                };
                header[18..20].copy_from_slice(&machine.to_le_bytes());
                file.write_all(&header).unwrap();
            }
            NativeFormat::MachO => {
                file.write_all(&0xfeed_facf_u32.to_be_bytes()).unwrap();
                let cpu = match target.architecture {
                    Architecture::X86_64 => 0x0100_0007_u32,
                    Architecture::Arm64 => 0x0100_000c_u32,
                };
                file.write_all(&cpu.to_be_bytes()).unwrap();
            }
        }
        #[cfg(unix)]
        make_executable(path);
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    fn create_directory_link(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn create_directory_link(target: &Path, link: &Path) {
        junction::create(target, link).unwrap();
    }
}
