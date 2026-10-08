//! Embedding hook for marsh `split { ... } | join`.
//!
//! Brush is a thin client: it spools the prefix, hands the branch sources to
//! the embedding (which asks the marsh daemon to snapshot, fork, run, and
//! capture every branch), then runs the stages after `join` with the split's
//! environment and releases the split with the last stage's status.

use std::path::Path;
use std::sync::{Arc, OnceLock};

/// One branch: its label and shell source.
#[derive(Clone, Debug)]
pub struct SplitBranchRequest {
    /// Branch label.
    pub label: String,
    /// The branch body as shell source.
    pub source: String,
}

/// A completed split, ready for `join`.
pub struct SplitRun {
    /// The split stage status (first nonzero branch status), or 130 when cancelled.
    pub status: u8,
    /// Whether the split was cancelled (join and later stages do not run).
    pub cancelled: bool,
    /// Bytes for the join stage's standard output (rendering or manifest).
    pub input: Vec<u8>,
    /// Exported environment for the stages after `join`.
    pub environment: Vec<(String, String)>,
    /// Release with the last stage's status and `--keep`; returns stderr lines.
    pub release: Box<dyn FnOnce(u8, bool) -> Vec<String> + Send>,
}

/// A started split: wait for it, or cancel it from another task.
pub trait SplitHandle: Send {
    /// A function that cancels the split (Ctrl-C at the shell).
    fn canceller(&self) -> Box<dyn Fn() + Send + Sync>;
    /// Block until every branch is captured or the split is cancelled.
    ///
    /// # Errors
    ///
    /// Returns a one-line diagnostic when the split failed.
    fn wait(self: Box<Self>) -> Result<SplitRun, String>;
}

/// The embedding's split provider.
pub trait SplitWorkspace: Send + Sync {
    /// Start every branch from `cwd` with `input` on stdin.
    ///
    /// # Errors
    ///
    /// Returns a one-line diagnostic for a setup failure (status 2).
    fn start(
        &self,
        cwd: &Path,
        branches: &[SplitBranchRequest],
        environment: Vec<(String, String)>,
        input: Vec<u8>,
        json: bool,
    ) -> Result<Box<dyn SplitHandle>, String>;
}

static SPLIT_WORKSPACE: OnceLock<Arc<dyn SplitWorkspace>> = OnceLock::new();

/// Installs the process-wide split provider. Only the first call takes effect.
pub fn install_split_workspace_hook(hook: Arc<dyn SplitWorkspace>) {
    let _ = SPLIT_WORKSPACE.set(hook);
}

pub(crate) fn workspace() -> Option<&'static Arc<dyn SplitWorkspace>> {
    SPLIT_WORKSPACE.get()
}
