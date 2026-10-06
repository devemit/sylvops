#![allow(unsafe_code)]

use std::time::Duration;
use std::{ffi::OsString, path::Path};

use sylvops_core::{domain::ProviderKind, provider::AuthenticationRequirement};
use sylvops_daemon::provider::ProviderRegistry;

#[tokio::test]
async fn native_claude_health_is_available_through_the_provider_registry() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let fake_claude = install_fake_claude(temporary.path());
    let _override = EnvironmentGuard::set("CLAUDE_CLI_PATH", &fake_claude);

    let registry = ProviderRegistry::new(None, &[ProviderKind::Claude]).expect("registry");
    let health = registry.probe(ProviderKind::Claude).await.expect("health");

    assert_eq!(health.kind, ProviderKind::Claude);
    assert!(health.available);
    assert!(health.authenticated);
    assert_eq!(health.version.as_deref(), Some("2.1.145"));
    assert_eq!(
        health.executable_path.as_deref(),
        std::fs::canonicalize(&fake_claude).unwrap().to_str()
    );
    assert_eq!(
        health.capabilities.authentication,
        AuthenticationRequirement::ExistingLogin
    );
    assert!(health.capabilities.interactive);
    assert!(health.capabilities.status_hooks);
    assert!(health.capabilities.model_selection);
    assert!(health.capabilities.effort_selection);
    assert!(health.diagnostic.is_none());

    assert_recovery_states(&registry, &fake_claude).await;
    assert_probe_bounds(&registry, &fake_claude).await;
    assert_missing_installation(temporary.path()).await;
}

async fn assert_recovery_states(registry: &ProviderRegistry, fake_claude: &Path) {
    write_fixture(fake_claude, "version", "2.1.144");
    let outdated = registry
        .probe(ProviderKind::Claude)
        .await
        .expect("outdated");
    assert!(!outdated.available);
    assert_eq!(outdated.version.as_deref(), Some("2.1.144"));
    assert!(outdated.diagnostic.as_deref().unwrap().contains("Update"));

    write_fixture(fake_claude, "version", "2.1.145");
    write_fixture(fake_claude, "auth", "logged-out");
    let logged_out = registry
        .probe(ProviderKind::Claude)
        .await
        .expect("logged out");
    assert!(logged_out.available);
    assert!(!logged_out.authenticated);
    assert!(
        logged_out
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("not logged in")
    );

    write_fixture(fake_claude, "auth", "unsupported");
    let unsupported = registry
        .probe(ProviderKind::Claude)
        .await
        .expect("unsupported");
    assert!(unsupported.available);
    assert!(!unsupported.authenticated);
    assert!(
        unsupported
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("unsupported authentication")
    );

    write_fixture(fake_claude, "auth", "console");
    let console = registry.probe(ProviderKind::Claude).await.expect("console");
    assert!(console.authenticated);

    write_fixture(fake_claude, "auth", "oauth");
    let oauth = registry.probe(ProviderKind::Claude).await.expect("OAuth");
    assert!(oauth.authenticated);

    write_fixture(fake_claude, "auth", "failed-supported");
    let failed = registry
        .probe(ProviderKind::Claude)
        .await
        .expect("failed status");
    assert!(!failed.authenticated);
    assert!(
        failed
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("could not be verified")
    );
}

async fn assert_probe_bounds(registry: &ProviderRegistry, fake_claude: &Path) {
    write_fixture(fake_claude, "auth", "malformed");
    let malformed = registry
        .probe(ProviderKind::Claude)
        .await
        .expect("malformed");
    assert!(!malformed.authenticated);
    assert!(
        !malformed
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("PRIVATE_AUTH_DETAIL")
    );

    write_fixture(fake_claude, "auth", "oversized");
    let oversized = registry
        .probe(ProviderKind::Claude)
        .await
        .expect("oversized");
    assert!(!oversized.authenticated);
    assert!(
        oversized
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("could not be verified")
    );

    write_fixture(fake_claude, "auth", "hang");
    let timed_out =
        tokio::time::timeout(Duration::from_secs(7), registry.probe(ProviderKind::Claude))
            .await
            .expect("health probe has a bounded timeout")
            .expect("timed-out health");
    assert!(!timed_out.authenticated);
    assert!(
        timed_out
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("could not be verified")
    );
}

async fn assert_missing_installation(temporary: &Path) {
    let missing_root = temporary.join("missing");
    std::fs::create_dir(&missing_root).unwrap();
    let _missing_override = EnvironmentGuard::set(
        "CLAUDE_CLI_PATH",
        missing_root.join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        }),
    );
    let _missing_path = EnvironmentGuard::set("PATH", &missing_root);
    let _missing_home = EnvironmentGuard::set("HOME", &missing_root);
    let _missing_profile = EnvironmentGuard::set("USERPROFILE", &missing_root);
    let _missing_local = EnvironmentGuard::set("LOCALAPPDATA", &missing_root);
    let missing_registry =
        ProviderRegistry::new(None, &[ProviderKind::Claude]).expect("missing registry");
    let missing = missing_registry
        .probe(ProviderKind::Claude)
        .await
        .expect("missing health");
    assert!(!missing.available);
    assert!(
        missing
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("not installed")
    );
}

fn write_fixture(executable: &Path, name: &str, value: &str) {
    std::fs::write(
        executable
            .parent()
            .unwrap()
            .join(format!("fake-claude-{name}")),
        value,
    )
    .unwrap();
}

fn install_fake_claude(directory: &Path) -> std::path::PathBuf {
    let target = directory.join(if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_fake-claude"), &target).expect("copy fake Claude");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(&target).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&target, permissions).unwrap();
    }
    target
}

struct EnvironmentGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentGuard {
    fn set(name: &'static str, value: impl Into<OsString>) -> Self {
        let previous = std::env::var_os(name);
        unsafe { std::env::set_var(name, value.into()) };
        Self { name, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(previous) = self.previous.take() {
                std::env::set_var(self.name, previous);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }
}
