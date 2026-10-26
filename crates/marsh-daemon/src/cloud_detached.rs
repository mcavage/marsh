//! Protocol for detached cloud runs (`docs/design/cloud-detach.md`): the
//! requests behind `marsh cloud attach|collect|diff|apply|discard|stop` and
//! the finish notices. The backend owns the records and the sandboxes.

use serde::{Deserialize, Serialize};

/// Most output or patch bytes in one reply (well inside a frame).
pub const DETACHED_CHUNK: usize = 128 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum CloudDetachedRequest {
    /// Collect now: read the run's state, fetch new output, and when it has
    /// finished, its changes (applied when safe, else kept as a patch).
    Collect { job: String },
    /// Stored output of one stream (1 stdout, 2 stderr) from `offset`.
    /// `refresh` first fetches new output from a running sandbox.
    Output {
        job: String,
        stream: u8,
        offset: u64,
        #[serde(default)]
        refresh: bool,
    },
    /// The kept patch as a reviewable diff against the files here, from
    /// byte `offset` of the rendered text.
    Diff { job: String, offset: u64 },
    /// Apply a kept patch anyway.
    Apply { job: String },
    /// Drop the job's sandbox and kept results.
    Discard { job: String },
    /// INT, then KILL after the grace period.
    Stop { job: String },
    /// `--detached` on `workers reset cloud`, `stop`, `reset`: discard every
    /// outstanding detached run.
    DiscardAll,
    /// Finish notices not shown yet (each is returned once).
    Notices,
    /// How many detached runs are outstanding (`total`).
    Outstanding,
}

/// One detached job as the CLI shows it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DetachedView {
    pub job_id: String,
    pub cursor: u64,
    /// `starting`, `running`, `paused`, `finished`, `collected`,
    /// `kept-as-patch`, `lost`, `failed`, or `discarded`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why it was kept as a patch, lost, or failed; or what happens next.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// The job is settled: nothing more will arrive.
    #[serde(default)]
    pub settled: bool,
    /// The run's oldest output was dropped in the sandbox (its spool cap).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub output_truncated: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CloudDetachedReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<DetachedView>,
    /// A line for the user (stderr).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Output or patch bytes starting at `offset`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bytes: Vec<u8>,
    #[serde(default)]
    pub offset: u64,
    /// Bytes available in total (output stored so far, or the patch text).
    #[serde(default)]
    pub total: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
}
