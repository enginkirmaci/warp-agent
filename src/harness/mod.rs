//! The app-level harness interface, registry, host and the compiled-in
//! opencode v1, dsh and opencode v2 adapters. The registry is the one list of
//! harnesses the settings page, the composer pill, the door and the model
//! pickers read; ids are wire strings ([`HarnessId`]), never an enum. Chat
//! traffic for opencode v1 and dsh still runs on the legacy net thread, which
//! launches its managed child from the descriptor's [`LaunchSpec`]; the
//! attached adapter (opencode v2) is routed through the host.

mod dsh;
mod host;
mod interface;
mod opencode;
mod opencode_v2;
mod registry;
mod shared;
mod types;

/// Test doubles for the harness seam: unit tests and probes use them to drive
/// the store and the views without a network or a child process.
#[doc(hidden)]
pub mod test_support;

pub use dsh::DshHarness;
pub use host::HarnessHost;
pub use interface::Harness;
pub use opencode::OpenCodeV1Harness;
pub use opencode_v2::OpenCodeV2Harness;
pub use registry::{HarnessDescriptor, HarnessRegistry};
pub use types::*;

/// The compiled-in registry, built once.
pub fn registry() -> &'static HarnessRegistry {
    static REGISTRY: std::sync::OnceLock<HarnessRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(HarnessRegistry::builtin)
}

/// The display name of a harness id.
pub fn label(id: &HarnessId) -> &'static str {
    registry().label_for(id)
}

/// The harness's one managed app-wide process (dsh), if it has one.
pub fn shared_server(id: &HarnessId) -> bool {
    registry().is_shared_server(id)
}

/// The harness attaches to a service the app does not own (opencode v2).
pub fn attached(id: &HarnessId) -> bool {
    registry().is_attached(id)
}

/// The harness has one app-wide instance rather than one per workspace.
pub fn app_scope(id: &HarnessId) -> bool {
    registry().is_app_scope(id)
}

/// The harness has one instance per workspace folder (opencode v1).
pub fn workspace_scoped(id: &HarnessId) -> bool {
    registry().is_workspace_scope(id)
}

/// The registry's one per-workspace harness (opencode v1).
pub fn workspace_harness() -> Option<HarnessId> {
    registry().workspace_id()
}

/// The registry's one attached harness (opencode v2).
pub fn attached_id() -> Option<HarnessId> {
    registry().attached_id()
}

/// The id of the registry's one managed app-wide harness (dsh).
pub fn shared_server_id() -> Option<HarnessId> {
    registry().shared_server_id()
}

/// The net key of the shared app-wide server: its harness id.
pub fn shared_server_key() -> String {
    shared_server_id()
        .map(|id| id.as_str().to_string())
        .unwrap_or_default()
}

/// Whether a net key names the shared app-wide server.
pub fn is_shared_key(key: &str) -> bool {
    registry()
        .shared_server_id()
        .is_some_and(|id| id.as_str() == key)
}

/// The key a harness instance or its net server is registered under.
pub fn instance_key(id: &HarnessId, folder: Option<&str>) -> String {
    registry().instance_key(id, folder)
}

/// The harness a net key names.
pub fn harness_for_key(key: &str) -> Option<HarnessId> {
    registry().harness_for_key(key)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::test_support::FakeHarness;
    use super::*;

    const FAKE_ID: &str = "fake";

    fn fake_harness_factory(_config: HarnessConfig) -> Box<dyn Harness> {
        Box::new(FakeHarness::new(FAKE_ID))
    }

    fn fake_descriptor() -> HarnessDescriptor {
        HarnessDescriptor {
            id: HarnessId::new(FAKE_ID),
            label: "fake",
            default_executable: "fake",
            connection: ConnectionKind::Managed,
            scope: InstanceScope::App,
            capabilities: HarnessCapabilities::NONE,
            launch: None,
            factory: Some(fake_harness_factory),
        }
    }

    #[test]
    fn builtin_registry_resolves_the_three_stable_ids() {
        let registry = HarnessRegistry::builtin();
        let ids: Vec<&str> = registry.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, vec!["opencode", "opencode-v2", "dsh"]);

        for wire in ["opencode", "opencode-v2", "dsh"] {
            let descriptor = registry.resolve(wire).expect("builtin harness");
            assert_eq!(descriptor.id.as_str(), wire);
        }
    }

    #[test]
    fn an_unknown_wire_id_is_a_typed_error() {
        let registry = HarnessRegistry::builtin();
        let error = registry.resolve("no-such-harness").unwrap_err();
        assert_eq!(
            error,
            HarnessError::UnknownHarness("no-such-harness".into())
        );
        assert!(registry.get(&HarnessId::new("no-such-harness")).is_none());
    }

    #[test]
    fn descriptors_carry_the_connection_and_capabilities_as_data() {
        let registry = HarnessRegistry::builtin();
        let v1 = registry.resolve("opencode").unwrap();
        let v2 = registry.resolve("opencode-v2").unwrap();
        let dsh = registry.resolve("dsh").unwrap();

        assert_eq!(v1.connection, ConnectionKind::Managed);
        assert_eq!(v2.connection, ConnectionKind::Attached);
        assert_eq!(dsh.connection, ConnectionKind::Managed);

        assert!(v1.capabilities.questions);
        assert!(v1.capabilities.skills);
        assert!(!v1.capabilities.forms);
        assert!(v2.capabilities.forms);
        assert!(v2.capabilities.inbox);
        assert!(v2.capabilities.multi_location);
        assert!(dsh.capabilities.skills);
        assert!(!dsh.capabilities.inbox);
        assert_eq!(v2.default_executable, "opencode");
        assert_eq!(v1.scope, InstanceScope::Workspace);
        assert_eq!(v2.scope, InstanceScope::App);
        assert!(v2.factory.is_some());
        assert!(dsh.factory.is_some());
    }

    #[test]
    fn the_v1_descriptor_builds_the_real_adapter() {
        let registry = HarnessRegistry::builtin();
        let factory = registry
            .resolve("opencode")
            .unwrap()
            .factory
            .expect("the v1 adapter has a factory");
        let mut adapter = factory(HarnessConfig::new("opencode"));
        assert_eq!(adapter.id().as_str(), "opencode");
        assert_eq!(adapter.health().state, HealthState::Starting);
        assert!(matches!(
            adapter.sessions().unwrap_err(),
            HarnessError::Unavailable(_)
        ));
    }

    #[test]
    fn the_dsh_descriptor_builds_the_real_adapter() {
        let registry = HarnessRegistry::builtin();
        let factory = registry
            .resolve("dsh")
            .unwrap()
            .factory
            .expect("the dsh adapter has a factory");
        let mut adapter = factory(HarnessConfig::new("dsh"));
        assert_eq!(adapter.id().as_str(), "dsh");
        assert_eq!(adapter.health().state, HealthState::Starting);
        assert!(matches!(
            adapter.sessions().unwrap_err(),
            HarnessError::Unavailable(_)
        ));
    }

    #[test]
    fn the_v2_descriptor_builds_the_real_adapter() {
        let registry = HarnessRegistry::builtin();
        let factory = registry
            .resolve("opencode-v2")
            .unwrap()
            .factory
            .expect("the v2 adapter has a factory");
        let mut adapter = factory(HarnessConfig::new("opencode"));
        assert_eq!(adapter.id().as_str(), "opencode-v2");
        assert_eq!(adapter.health().state, HealthState::Starting);
        assert!(matches!(
            adapter.sessions().unwrap_err(),
            HarnessError::Unavailable(_)
        ));
    }

    #[test]
    fn the_host_keeps_one_v1_instance_per_workspace() {
        let registry = HarnessRegistry::builtin();
        let factory = registry
            .resolve("opencode")
            .unwrap()
            .factory
            .expect("the v1 adapter has a factory");
        let mut host = HarnessHost::new(registry);
        let id = HarnessId::new("opencode");
        for folder in ["/tmp/warp-agent-a", "/tmp/warp-agent-b"] {
            let mut config = HarnessConfig::new("opencode");
            config.directory = Some(std::path::PathBuf::from(folder));
            host.register_instance(factory(config)).unwrap();
        }
        assert!(
            host.instance_at(&id, Some(std::path::Path::new("/tmp/warp-agent-a")))
                .is_some()
        );
        assert!(
            host.instance_at(&id, Some(std::path::Path::new("/tmp/warp-agent-b")))
                .is_some()
        );
        let mut duplicate = HarnessConfig::new("opencode");
        duplicate.directory = Some(std::path::PathBuf::from("/tmp/warp-agent-a"));
        assert!(matches!(
            host.register_instance(factory(duplicate)).unwrap_err(),
            HarnessError::DuplicateInstance(_)
        ));
    }

    #[test]
    fn a_fake_adapter_registers_and_unsupported_is_typed() {
        let mut registry = HarnessRegistry::new();
        registry.register(fake_descriptor()).unwrap();

        let descriptor = registry.resolve(FAKE_ID).unwrap();
        let factory = descriptor.factory.expect("the fake has a factory");
        let mut harness = factory(HarnessConfig::new("fake"));

        assert_eq!(harness.id().as_str(), FAKE_ID);
        assert_eq!(harness.health().state, HealthState::Ready);
        assert_eq!(
            harness.models().unwrap_err(),
            HarnessError::Unsupported("models")
        );
        assert_eq!(
            harness.inbox(&SessionId::new("session-1")).unwrap_err(),
            HarnessError::Unsupported("inbox")
        );
    }

    #[test]
    fn a_duplicate_registration_is_a_typed_error() {
        let mut registry = HarnessRegistry::new();
        registry.register(fake_descriptor()).unwrap();
        let error = registry.register(fake_descriptor()).unwrap_err();
        assert_eq!(
            error,
            HarnessError::DuplicateHarness(HarnessId::new(FAKE_ID))
        );
    }

    #[test]
    fn the_host_registers_starts_and_shuts_down_an_instance() {
        let mut registry = HarnessRegistry::new();
        registry.register(fake_descriptor()).unwrap();

        let mut host = HarnessHost::new(registry);
        let started = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        host.register_instance(Box::new(FakeHarness::with_flags(
            FAKE_ID,
            started.clone(),
            stopped.clone(),
        )))
        .unwrap();

        let id = HarnessId::new(FAKE_ID);
        host.with_instance_mut(&id, |harness| harness.start())
            .unwrap()
            .unwrap();
        assert!(started.load(Ordering::SeqCst));
        assert!(host.instance(&id).is_some());

        assert!(host.shutdown().is_empty());
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn the_host_rejects_an_unknown_harness() {
        let mut host = HarnessHost::new(HarnessRegistry::new());
        let error = host
            .register_instance(Box::new(FakeHarness::new(FAKE_ID)))
            .unwrap_err();
        assert_eq!(error, HarnessError::UnknownHarness(FAKE_ID.into()));
    }

    #[test]
    fn the_host_rejects_a_duplicate_instance() {
        let mut registry = HarnessRegistry::new();
        registry.register(fake_descriptor()).unwrap();

        let mut host = HarnessHost::new(registry);
        host.register_instance(Box::new(FakeHarness::new(FAKE_ID)))
            .unwrap();
        let error = host
            .register_instance(Box::new(FakeHarness::new(FAKE_ID)))
            .unwrap_err();
        assert!(matches!(error, HarnessError::DuplicateInstance(_)));
    }

    #[test]
    fn instance_keys_follow_the_descriptor_scope() {
        let registry = HarnessRegistry::builtin();
        let workspace = HarnessId::new("opencode");
        let shared = HarnessId::new("dsh");
        let attached = HarnessId::new("opencode-v2");

        // a per-workspace harness is keyed by its folder
        assert_eq!(
            registry.instance_key(&workspace, Some("/tmp/repo")),
            "/tmp/repo"
        );
        // an app-wide one is keyed by its id, whatever folder it is asked for
        assert_eq!(registry.instance_key(&shared, Some("/tmp/repo")), "dsh");
        assert_eq!(registry.instance_key(&attached, None), "opencode-v2");

        // and a net key resolves back to the harness it names
        assert_eq!(registry.harness_for_key("dsh"), Some(shared));
        assert_eq!(registry.harness_for_key("opencode-v2"), Some(attached));
        assert_eq!(registry.harness_for_key("/tmp/repo"), Some(workspace));
    }

    #[test]
    fn scope_and_connection_predicates_come_from_the_descriptors() {
        let registry = HarnessRegistry::builtin();
        let workspace = HarnessId::new("opencode");
        let shared = HarnessId::new("dsh");
        let attached = HarnessId::new("opencode-v2");

        assert!(registry.is_workspace_scope(&workspace));
        assert!(!registry.is_app_scope(&workspace));
        assert!(registry.is_shared_server(&shared));
        assert!(!registry.is_shared_server(&attached));
        assert!(registry.is_attached(&attached));
        assert!(!registry.is_attached(&shared));
        assert_eq!(registry.shared_server_id(), Some(shared));
        assert_eq!(registry.workspace_id(), Some(workspace));
        assert_eq!(registry.attached_id(), Some(attached.clone()));
        assert_eq!(registry.label_for(&attached), "opencode v2");
        assert_eq!(
            registry.label_for(&HarnessId::new("no-such-harness")),
            "unknown harness"
        );
    }

    #[test]
    fn a_harness_id_roundtrips_through_json_as_its_wire_string() {
        let id = HarnessId::new("opencode-v2");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"opencode-v2\"");
        let back: HarnessId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
