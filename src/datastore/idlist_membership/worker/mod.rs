// Implementation is split by recovery boundary so reviewers can reason about
// checkpoint ordering independently for download, diff, and removal phases.
mod core;
mod delta;
mod download;
#[cfg(test)]
mod integration_tests;
mod parser;
mod reload;
mod removal;
mod run;
mod streamed_diff;

pub use core::{
    IdListSyncWorker, IdListWorkerConfig, TaskMetadata, TaskOp, WorkerAction, WorkerError,
    WorkerRunOutcome,
};
