//! Deterministic comparisons for Tachyon's update/rollback preview.
use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A single map change, including metadata needed for a preview.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Change<T> {
    /// Stable key or config path.
    pub key: String,
    /// Previous value, absent on add.
    pub before: Option<T>,
    /// Target value, absent on removal.
    pub after: Option<T>,
}
/// Update/rollback plan; applying files remains the launcher's responsibility.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpdatePlan {
    /// Changed game/loader/pack metadata, if any.
    pub pack: Option<Change<Pack>>,
    /// Mod changes in stable key order.
    pub mods: Vec<Change<ModEntry>>,
    /// Config changes in stable path order. Local user-file conflicts need UI resolution.
    pub configs: Vec<Change<FileSpec>>,
}
fn changes<T: Clone + PartialEq>(
    a: &BTreeMap<String, T>,
    b: &BTreeMap<String, T>,
) -> Vec<Change<T>> {
    a.keys()
        .chain(b.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| Change {
            key: k.clone(),
            before: a.get(k).cloned(),
            after: b.get(k).cloned(),
        })
        .collect()
}
/// Compare installed metadata with a target; reverse arguments to preview undo.
pub fn between(installed: &Manifest, target: &Manifest) -> UpdatePlan {
    UpdatePlan {
        pack: (installed.pack != target.pack).then(|| Change {
            key: target.pack.id.clone(),
            before: Some(installed.pack.clone()),
            after: Some(target.pack.clone()),
        }),
        mods: changes(&installed.mods, &target.mods),
        configs: changes(&installed.configs, &target.configs),
    }
}
