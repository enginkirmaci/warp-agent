//! The compile-time harness registry.
//!
//! Adding a harness is one descriptor entry here plus its adapter module; the
//! settings page, the composer pill, the door's harness list and the model
//! pickers iterate this table rather than matching on an enum.

use super::dsh::DshHarness;
use super::interface::Harness;
use super::opencode::OpenCodeV1Harness;
use super::opencode_v2::OpenCodeV2Harness;
use super::types::{
    ConnectionKind, HarnessCapabilities, HarnessConfig, HarnessError, HarnessId, InstanceScope,
    LaunchSpec,
};

/// One row of the registry: what the harness is called, how the app reaches
/// it, what it supports and how an instance is made.
#[derive(Clone, Debug)]
pub struct HarnessDescriptor {
    pub id: HarnessId,
    pub label: &'static str,
    pub default_executable: &'static str,
    pub connection: ConnectionKind,
    /// Whether the harness has one app-wide instance or one per workspace.
    pub scope: InstanceScope,
    pub capabilities: HarnessCapabilities,
    /// How the managed child is launched; `None` for an attached harness.
    pub launch: Option<LaunchSpec>,
    /// `None` until the adapter ticket fills the factory; the registry still
    /// resolves the id, label, connection and capabilities in the expand phase.
    pub factory: Option<fn(HarnessConfig) -> Box<dyn Harness>>,
}

/// The ordered list of harnesses the app can run.
#[derive(Clone, Debug, Default)]
pub struct HarnessRegistry {
    entries: Vec<HarnessDescriptor>,
}

impl HarnessRegistry {
    /// An empty registry, for tests and for a host that is handed entries.
    pub fn new() -> Self {
        Self::default()
    }

    /// The app's compiled-in harness list.
    ///
    /// The labels and wire ids deliberately match the old harness enum during
    /// the expand phase; the contract ticket removes that enum and the door and
    /// settings move to this table together.
    pub fn builtin() -> Self {
        Self {
            entries: vec![
                opencode_descriptor(),
                opencode_v2_descriptor(),
                dsh_descriptor(),
            ],
        }
    }

    /// Add one descriptor; a duplicate id is a typed error.
    pub fn register(&mut self, descriptor: HarnessDescriptor) -> Result<(), HarnessError> {
        if self.entries.iter().any(|entry| entry.id == descriptor.id) {
            return Err(HarnessError::DuplicateHarness(descriptor.id.clone()));
        }
        self.entries.push(descriptor);
        Ok(())
    }

    /// Look one harness up by its id.
    pub fn get(&self, id: &HarnessId) -> Option<&HarnessDescriptor> {
        self.resolve(id.as_str()).ok()
    }

    /// Resolve a wire id, or report a typed unknown-harness error.
    pub fn resolve(&self, wire: &str) -> Result<&HarnessDescriptor, HarnessError> {
        self.entries
            .iter()
            .find(|entry| entry.id.as_str() == wire)
            .ok_or_else(|| HarnessError::UnknownHarness(wire.to_string()))
    }

    /// Every registered descriptor, in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &HarnessDescriptor> {
        self.entries.iter()
    }

    /// The first registered harness: the app's fallback backend.
    pub fn default_id(&self) -> HarnessId {
        self.entries
            .first()
            .map(|entry| entry.id.clone())
            .unwrap_or_else(|| HarnessId::new("opencode"))
    }

    /// The one managed app-wide harness (dsh), when the registry has one.
    pub fn shared_server_id(&self) -> Option<HarnessId> {
        self.entries
            .iter()
            .find(|entry| {
                entry.scope == InstanceScope::App && entry.connection == ConnectionKind::Managed
            })
            .map(|entry| entry.id.clone())
    }

    /// The one attached harness (opencode v2), when the registry has one.
    pub fn attached_id(&self) -> Option<HarnessId> {
        self.entries
            .iter()
            .find(|entry| entry.connection == ConnectionKind::Attached)
            .map(|entry| entry.id.clone())
    }

    /// The one per-workspace harness, when the registry has one.
    pub fn workspace_id(&self) -> Option<HarnessId> {
        self.entries
            .iter()
            .find(|entry| entry.scope == InstanceScope::Workspace)
            .map(|entry| entry.id.clone())
    }

    /// What a picker calls the harness; an unknown id keeps a stable word.
    pub fn label_for(&self, id: &HarnessId) -> &'static str {
        self.get(id).map_or("unknown harness", |entry| entry.label)
    }

    /// Whether the harness has one app-wide instance.
    pub fn is_app_scope(&self, id: &HarnessId) -> bool {
        self.get(id)
            .is_some_and(|entry| entry.scope == InstanceScope::App)
    }

    /// Whether the harness is one managed app-wide process (dsh).
    pub fn is_shared_server(&self, id: &HarnessId) -> bool {
        self.get(id).is_some_and(|entry| {
            entry.scope == InstanceScope::App && entry.connection == ConnectionKind::Managed
        })
    }

    /// Whether the harness has one instance per workspace folder (v1).
    pub fn is_workspace_scope(&self, id: &HarnessId) -> bool {
        self.get(id)
            .is_some_and(|entry| entry.scope == InstanceScope::Workspace)
    }

    /// Whether the app attaches to a service it does not manage (v2).
    pub fn is_attached(&self, id: &HarnessId) -> bool {
        self.get(id)
            .is_some_and(|entry| entry.connection == ConnectionKind::Attached)
    }

    /// The key one harness instance is registered under: the id for an
    /// app-wide harness, the workspace folder for a per-workspace one.
    pub fn instance_key(&self, id: &HarnessId, folder: Option<&str>) -> String {
        if self.is_app_scope(id) {
            id.as_str().to_string()
        } else {
            folder.unwrap_or_default().to_string()
        }
    }

    /// Which harness a net key names: an app-wide harness by its id, anything
    /// else by the registry's per-workspace harness.
    pub fn harness_for_key(&self, key: &str) -> Option<HarnessId> {
        if let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.scope == InstanceScope::App && entry.id.as_str() == key)
        {
            return Some(entry.id.clone());
        }
        self.workspace_id()
    }
}

fn descriptor(
    id: &str,
    label: &'static str,
    executable: &'static str,
    connection: ConnectionKind,
    capabilities: HarnessCapabilities,
) -> HarnessDescriptor {
    HarnessDescriptor {
        id: HarnessId::new(id),
        label,
        default_executable: executable,
        connection,
        scope: InstanceScope::App,
        capabilities,
        launch: None,
        factory: None,
    }
}

fn opencode_descriptor() -> HarnessDescriptor {
    let mut descriptor = descriptor(
        "opencode",
        "opencode",
        "opencode",
        ConnectionKind::Managed,
        HarnessCapabilities::OPENCODE_V1,
    );
    descriptor.scope = InstanceScope::Workspace;
    descriptor.factory = Some(opencode_factory);
    descriptor.launch = Some(super::opencode::launch_spec());
    descriptor
}

fn opencode_factory(config: HarnessConfig) -> Box<dyn Harness> {
    Box::new(OpenCodeV1Harness::new(config))
}

fn opencode_v2_descriptor() -> HarnessDescriptor {
    let mut descriptor = descriptor(
        "opencode-v2",
        "opencode v2",
        "opencode",
        ConnectionKind::Attached,
        HarnessCapabilities::OPENCODE_V2,
    );
    descriptor.factory = Some(opencode_v2_factory);
    descriptor
}

fn opencode_v2_factory(config: HarnessConfig) -> Box<dyn Harness> {
    Box::new(OpenCodeV2Harness::new(config))
}

fn dsh_descriptor() -> HarnessDescriptor {
    let mut descriptor = descriptor(
        "dsh",
        "DeepSeek (dsh)",
        "dsh",
        ConnectionKind::Managed,
        HarnessCapabilities::DSH,
    );
    descriptor.factory = Some(dsh_factory);
    descriptor.launch = Some(super::dsh::launch_spec());
    descriptor
}

fn dsh_factory(config: HarnessConfig) -> Box<dyn Harness> {
    Box::new(DshHarness::new(config))
}
