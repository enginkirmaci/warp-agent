//! The harness host: the one place that owns harness instances and routes by
//! stable id and instance scope.
//!
//! A per-workspace harness (opencode v1) has one instance per folder; an
//! app-wide harness (dsh, opencode v2) has exactly one. The host keys instances
//! by that scope, so the registry's scope fact is enforced here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::interface::Harness;
use super::registry::HarnessRegistry;
use super::types::{HarnessError, HarnessId, InstanceScope};

type InstanceKey = (HarnessId, Option<PathBuf>);

/// Routes operations to the harness instance registered under an id.
pub struct HarnessHost {
    registry: HarnessRegistry,
    instances: BTreeMap<InstanceKey, Box<dyn Harness>>,
}

impl HarnessHost {
    pub fn new(registry: HarnessRegistry) -> Self {
        Self {
            registry,
            instances: BTreeMap::new(),
        }
    }

    /// Take ownership of one instance. The id must be in the registry, the
    /// instance must match the descriptor's scope, and the key must be free.
    pub fn register_instance(&mut self, harness: Box<dyn Harness>) -> Result<(), HarnessError> {
        let id = harness.id();
        let Some(descriptor) = self.registry.get(&id) else {
            return Err(HarnessError::UnknownHarness(id.to_string()));
        };
        let scope = harness.scope();
        if descriptor.scope == InstanceScope::Workspace && scope.is_none() {
            return Err(HarnessError::Failed(format!(
                "harness {id} needs a workspace directory"
            )));
        }
        let key = (
            id.clone(),
            if descriptor.scope == InstanceScope::Workspace {
                scope
            } else {
                None
            },
        );
        if self.instances.contains_key(&key) {
            return Err(HarnessError::DuplicateInstance(id));
        }
        self.instances.insert(key, harness);
        Ok(())
    }

    /// The first instance for an id, if one is registered. App-wide harnesses
    /// have exactly one; a workspace harness's first folder is a valid default.
    pub fn instance(&self, id: &HarnessId) -> Option<&dyn Harness> {
        self.instances
            .iter()
            .find(|((harness, _), _)| harness == id)
            .map(|(_, instance)| instance.as_ref())
    }

    /// The instance for an id and workspace scope, if registered.
    pub fn instance_at(&self, id: &HarnessId, scope: Option<&Path>) -> Option<&dyn Harness> {
        self.instances
            .get(&(id.clone(), scope.map(Path::to_path_buf)))
            .map(|instance| instance.as_ref())
    }

    /// Run `f` against the first instance for an id, if one is registered.
    pub fn with_instance_mut<R>(
        &mut self,
        id: &HarnessId,
        f: impl FnOnce(&mut dyn Harness) -> R,
    ) -> Option<R> {
        self.instances
            .iter_mut()
            .find(|((harness, _), _)| harness == id)
            .map(|(_, instance)| f(instance.as_mut()))
    }

    /// Run `f` against the instance for an id and workspace scope.
    pub fn with_instance_mut_at<R>(
        &mut self,
        id: &HarnessId,
        scope: Option<&Path>,
        f: impl FnOnce(&mut dyn Harness) -> R,
    ) -> Option<R> {
        self.instances
            .get_mut(&(id.clone(), scope.map(Path::to_path_buf)))
            .map(|instance| f(instance.as_mut()))
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
