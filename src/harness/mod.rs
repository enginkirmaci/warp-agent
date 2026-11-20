//! The app-level harness interface, registry and host.
//!
//! This is the expand-phase scaffold: the old opencode and dsh paths still run
//! untouched. The adapters, the full typed event vocabulary and the store/door
//! wiring land in the following tickets.

mod host;
mod interface;
mod opencode;
mod registry;
mod types;

/// Test doubles for the harness seam: unit tests and probes use them to drive
/// the store and the views without a network or a child process.
#[doc(hidden)]
pub mod test_support;

pub use host::HarnessHost;
pub use interface::Harness;
pub use opencode::OpenCodeV1Harness;
pub use registry::{HarnessDescriptor, HarnessRegistry};
pub use types::*;

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
        assert!(v2.factory.is_none());
        assert!(dsh.factory.is_none());
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
    fn a_harness_id_roundtrips_through_json_as_its_wire_string() {
        let id = HarnessId::new("opencode-v2");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"opencode-v2\"");
        let back: HarnessId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
