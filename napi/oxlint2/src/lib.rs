use oxlint::lint as oxlint_lint;
pub use oxlint::{ExternalLinter, ExternalLinterCb, ExternalLinterLoadPluginCb};

use napi_derive::napi;
use std::process::{ExitCode, Termination};

#[napi]
pub fn lint(run: ExternalLinterCb, load_plugin: ExternalLinterLoadPluginCb) -> bool {
    oxlint_lint(Some(ExternalLinter::new(run, load_plugin))).report() == ExitCode::SUCCESS
}
