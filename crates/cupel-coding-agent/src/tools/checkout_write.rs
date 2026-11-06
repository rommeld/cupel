//! Shared checkout locks for independent sessions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

pub type WriteGate = Arc<tokio::sync::Mutex<()>>;
#[derive(Default)]
pub struct CheckoutGates {
    gates: BTreeMap<PathBuf, Weak<tokio::sync::Mutex<()>>>,
}
impl CheckoutGates {
    pub fn gate(&mut self, cwd: &Path) -> WriteGate {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let key = cwd
            .ancestors()
            .find(|path| path.join(".git").exists())
            .unwrap_or(&cwd)
            .to_path_buf();
        self.gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = self.gates.get(&key).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        self.gates.insert(key, Arc::downgrade(&gate));
        gate
    }
}
