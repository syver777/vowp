//! Built-in adapter construction and adapter-registry wiring.
//! Spec: `docs/design/crates/vowp-core.md` §2 and `docs/design/adapters.md` §10.

use vowp_adapters::acp::{AcpAdapter, AcpConfig};
use vowp_protocol::VowpConfig;

/// Builds the ACP adapter from `[harnesses.acp]`.
///
/// A missing table uses [`AcpConfig::default`] (`claude-code-acp` on `PATH`). The adapter's
/// extra arguments come from the harness table, like the other built-in adapters.
pub(crate) fn acp_adapter(config: &VowpConfig) -> AcpAdapter {
    let harness = config.harnesses.get("acp").cloned().unwrap_or_default();
    AcpAdapter::new(AcpConfig {
        binary: harness.binary,
        extra_args: harness.extra_args,
        ..AcpConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use vowp_protocol::{Executor, HarnessConfig, HarnessKind};

    use super::*;
    use crate::deps::AdapterRegistry;

    fn acp_executor() -> Executor {
        Executor {
            harness: HarnessKind::Acp,
            model: "claude-code-acp".to_owned(),
            effort: None,
            variant: None,
        }
    }

    #[test]
    fn registry_maps_acp_harness() {
        let config = VowpConfig::with_defaults_for_tests();
        let registry = AdapterRegistry::from_config(&config).expect("registry builds");
        let adapter = registry.get(&acp_executor()).expect("ACP is registered");
        assert_eq!(adapter.kind(), HarnessKind::Acp);
    }

    #[test]
    fn registry_uses_the_configured_acp_binary() {
        let directory = std::env::temp_dir().join(format!("vowp-acp-registry-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("create temp directory");
        let binary = directory.join("fake-acp");
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").expect("write fake binary");
        let mut permissions = std::fs::metadata(&binary).expect("stat fake binary").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&binary, permissions).expect("chmod fake binary");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.harnesses.insert(
            "acp".to_owned(),
            HarnessConfig {
                binary: Some(binary.to_string_lossy().into_owned()),
                ..HarnessConfig::default()
            },
        );
        let registry = AdapterRegistry::from_config(&config).expect("registry builds");
        let probe = registry.get(&acp_executor()).expect("ACP is registered").probe();
        assert!(probe.ok, "{}", probe.detail);
        assert_eq!(probe.path.as_deref(), Some(binary.to_string_lossy().as_ref()));

        let _ = std::fs::remove_dir_all(&directory);
    }
}
