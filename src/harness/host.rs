//! The harness host: the one place that owns runtime harness instances and
//! routes by stable id and instance scope.
//!
//! A per-workspace harness (opencode v1) has one instance per folder; an
//! app-wide harness (dsh, opencode v2) has exactly one. Instances are created
//! through the registry descriptor's factory, so the host never names a
//! harness; the descriptor's scope fact is enforced here.
//!
//! Each instance sits behind its own lock: starting one workspace's server
//! (25 s on opencode) must not block a request against another workspace's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::interface::Harness;
use super::registry::HarnessRegistry;
use super::types::{HarnessConfig, HarnessError, HarnessId, InstanceScope};

type InstanceKey = (HarnessId, Option<PathBuf>);

/// A shared handle to one registered instance.
pub type HarnessHandle = Arc<Mutex<Box<dyn Harness>>>;

/// Owns the running harness instances and dispatches operations by id scope.
pub struct HarnessHost {
    registry: HarnessRegistry,
    instances: BTreeMap<InstanceKey, HarnessHandle>,
}

impl HarnessHost {
    pub fn new(registry: HarnessRegistry) -> Self {
        Self {
            registry,
            instances: BTreeMap::new(),
        }
    }

    /// The registry the host creates instances from.
    pub fn registry(&self) -> &HarnessRegistry {
        &self.registry
    }

    /// The scope key a descriptor gives an instance.
    fn scope_for(
        &self,
        id: &HarnessId,
        directory: Option<&Path>,
    ) -> Result<Option<PathBuf>, HarnessError> {
        let Some(descriptor) = self.registry.get(id) else {
            return Err(HarnessError::UnknownHarness(id.to_string()));
        };
        if descriptor.scope == InstanceScope::Workspace {
            let directory = directory.map(Path::to_path_buf);
            if directory.is_none() {
                return Err(HarnessError::Failed(format!(
                    "harness {id} needs a workspace directory"
                )));
            }
            return Ok(directory);
        }
        Ok(None)
    }

    /// Create the instance for an id through its registry factory when the
    /// host does not own one yet, and hand back the shared handle.
    pub fn ensure_instance(
        &mut self,
        id: &HarnessId,
        executable: &str,
        directory: Option<&Path>,
    ) -> Result<HarnessHandle, HarnessError> {
        let scope = self.scope_for(id, directory)?;
        if let Some(handle) = self.instances.get(&(id.clone(), scope.clone())) {
            return Ok(handle.clone());
        }
        let Some(descriptor) = self.registry.get(id) else {
            return Err(HarnessError::UnknownHarness(id.to_string()));
        };
        let Some(factory) = descriptor.factory else {
            return Err(HarnessError::Unavailable(format!(
                "harness {id} has no adapter"
            )));
        };
        let mut config = HarnessConfig::new(executable);
        config.directory = scope.clone();
        let harness = factory(config);
        if &harness.id() != id {
            return Err(HarnessError::Failed(format!(
                "adapter for {id} reports itself as {}",
                harness.id()
            )));
        }
        let handle: HarnessHandle = Arc::new(Mutex::new(harness));
        self.instances
            .insert((id.clone(), scope), Arc::clone(&handle));
        Ok(handle)
    }

    /// Take ownership of one instance the caller already built. The id must
    /// be in the registry, the instance must match the descriptor's scope and
    /// the key must be free.
    pub fn register_instance(&mut self, harness: Box<dyn Harness>) -> Result<(), HarnessError> {
        let id = harness.id();
        let scope = self.scope_for(&id, harness.scope().as_deref())?;
        if self.instances.contains_key(&(id.clone(), scope.clone())) {
            return Err(HarnessError::DuplicateInstance(id));
        }
        self.instances
            .insert((id.clone(), scope), Arc::new(Mutex::new(harness)));
        Ok(())
    }

    /// The first instance for an id, if one is registered. App-wide harnesses
    /// have exactly one; a workspace harness's first folder is a valid default.
    pub fn handle(&self, id: &HarnessId) -> Option<HarnessHandle> {
        self.instances
            .iter()
            .find(|((harness, _), _)| harness == id)
            .map(|(_, handle)| Arc::clone(handle))
    }

    /// The handle for an id and workspace scope, if registered.
    pub fn handle_at(&self, id: &HarnessId, scope: Option<&Path>) -> Option<HarnessHandle> {
        self.instances
            .get(&(id.clone(), scope.map(Path::to_path_buf)))
            .map(Arc::clone)
    }

    /// Whether an instance is registered for an id.
    pub fn contains(&self, id: &HarnessId) -> bool {
        self.instances.keys().any(|(harness, _)| harness == id)
    }

    /// Whether an instance is registered for an id and workspace scope.
    pub fn contains_at(&self, id: &HarnessId, scope: Option<&Path>) -> bool {
        self.instances
            .contains_key(&(id.clone(), scope.map(Path::to_path_buf)))
    }

    /// Stop and hand back one instance; the caller drops the handle to end it.
    pub fn remove_instance_at(
        &mut self,
        id: &HarnessId,
        scope: Option<&Path>,
    ) -> Option<HarnessHandle> {
        self.instances
            .remove(&(id.clone(), scope.map(Path::to_path_buf)))
    }

    /// Run `f` against the first instance for an id, if one is registered.
    pub fn with_instance_mut<R>(
        &mut self,
        id: &HarnessId,
        f: impl FnOnce(&mut dyn Harness) -> R,
    ) -> Option<R> {
        let handle = self.handle(id)?;
        let mut guard = handle.lock().expect("harness instance");
        Some(f(guard.as_mut()))
    }

    /// Run `f` against the instance for an id and workspace scope.
    pub fn with_instance_mut_at<R>(
        &mut self,
        id: &HarnessId,
        scope: Option<&Path>,
        f: impl FnOnce(&mut dyn Harness) -> R,
    ) -> Option<R> {
        let handle = self.handle_at(id, scope)?;
        let mut guard = handle.lock().expect("harness instance");
        Some(f(guard.as_mut()))
    }

    /// Stop every registered instance, collecting the failures.
    pub fn shutdown(&mut self) -> Vec<HarnessError> {
        let instances: Vec<HarnessHandle> = self.instances.values().cloned().collect();
        self.instances.clear();
        instances
            .into_iter()
            .filter_map(|handle| {
                let mut harness = handle.lock().expect("harness instance");
                harness.stop().err()
            })
            .collect()
    }
}

impl Drop for HarnessHost {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
