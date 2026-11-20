//! The harness host: the one place that owns harness instances and routes by
//! stable id.
//!
//! This is the expand-phase scaffold. It stores instances and shuts them down;
//! the net runtime, the enabled state and the per-scope instance keys land with
//! the adapter and settings tickets.

use std::collections::BTreeMap;

use super::interface::Harness;
use super::registry::HarnessRegistry;
use super::types::{HarnessError, HarnessId};

/// Routes operations to the harness instance registered under an id.
pub struct HarnessHost {
    registry: HarnessRegistry,
    instances: BTreeMap<HarnessId, Box<dyn Harness>>,
}

impl HarnessHost {
    pub fn new(registry: HarnessRegistry) -> Self {
        Self {
            registry,
            instances: BTreeMap::new(),
        }
    }

    /// Take ownership of one instance. The id must be in the registry and must
    /// not already have an instance.
    pub fn register_instance(&mut self, harness: Box<dyn Harness>) -> Result<(), HarnessError> {
        let id = harness.id();
        if self.registry.get(&id).is_none() {
            return Err(HarnessError::UnknownHarness(id.to_string()));
        }
        if self.instances.contains_key(&id) {
            return Err(HarnessError::DuplicateInstance(id));
        }
        self.instances.insert(id, harness);
        Ok(())
    }

    /// The instance for an id, if one is registered.
    pub fn instance(&self, id: &HarnessId) -> Option<&dyn Harness> {
        self.instances.get(id).map(|harness| harness.as_ref())
    }

    /// Run `f` against the instance for an id, if one is registered.
    pub fn with_instance_mut<R>(
        &mut self,
        id: &HarnessId,
        f: impl FnOnce(&mut dyn Harness) -> R,
    ) -> Option<R> {
        self.instances
            .get_mut(id)
            .map(|harness| f(harness.as_mut()))
    }

    /// Stop every registered instance, collecting the failures.
    pub fn shutdown(&mut self) -> Vec<HarnessError> {
        self.instances
            .values_mut()
            .filter_map(|harness| harness.stop().err())
            .collect()
    }
}

impl Drop for HarnessHost {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
