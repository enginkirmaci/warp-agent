//! The compile-time harness registry.
//!
//! Adding a harness is one descriptor entry here plus its adapter module; the
//! settings page, the composer pill, the door's harness list and the model
//! pickers iterate this table rather than matching on an enum.

use super::dsh::DshHarness;
use super::interface::Harness;
use super::opencode::OpenCodeV1Harness;
use super::types::{
    ConnectionKind, HarnessCapabilities, HarnessConfig, HarnessError, HarnessId, InstanceScope,
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
                descriptor(
                    "opencode-v2",
                    "opencode v2",
                    "opencode",
                    ConnectionKind::Attached,
                    HarnessCapabilities::OPENCODE_V2,
                ),
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
    descriptor
}

fn opencode_factory(config: HarnessConfig) -> Box<dyn Harness> {
    Box::new(OpenCodeV1Harness::new(config))
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
    descriptor
}

fn dsh_factory(config: HarnessConfig) -> Box<dyn Harness> {
    Box::new(DshHarness::new(config))
}
