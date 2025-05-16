use std::{str::FromStr, sync::Arc, vec};

use log::debug;
use rustc_hash::FxBuildHasher;
use tokio::sync::{Mutex, RwLock};
use tower_lsp_server::{
    UriExt,
    lsp_types::{CodeActionOrCommand, Diagnostic, FileEvent, Range, TextEdit, Uri},
};

use crate::{
    ConcurrentHashMap, Options, Run,
    code_actions::{
        apply_all_fix_code_action, apply_fix_code_action, ignore_this_line_code_action,
        ignore_this_rule_code_action,
    },
    linter::{error_with_position::DiagnosticReport, server_linter::ServerLinter},
};

pub struct WorkspaceWorker {
    root_uri: Uri,
    server_linter: RwLock<Option<ServerLinter>>,
    diagnostics_report_map: Arc<ConcurrentHashMap<String, Vec<DiagnosticReport>>>,
    options: Mutex<Options>,
}

// When changing configuration or watched files
pub struct ClientNotifications {
    // the diagnostics are changing for some files
    pub diagnostics: Option<ConcurrentHashMap<String, Vec<DiagnosticReport>>>,
    // the configuration changed `configPath` or some watched files changed `extends`
    #[expect(dead_code)]
    pub add_file_watchers: Option<()>,
    // the configuration changed `configPath` or some watched files changed `extends`
    #[expect(dead_code)]
    pub remove_file_watchers: Option<()>,
}

impl WorkspaceWorker {
    pub fn new(root_uri: Uri) -> Self {
        Self {
            root_uri,
            server_linter: RwLock::new(None),
            diagnostics_report_map: Arc::new(ConcurrentHashMap::default()),
            options: Mutex::new(Options::default()),
        }
    }

    pub fn get_root_uri(&self) -> &Uri {
        &self.root_uri
    }

    pub fn is_responsible_for_uri(&self, uri: &Uri) -> bool {
        if let Some(path) = uri.to_file_path() {
            return path.starts_with(self.root_uri.to_file_path().unwrap());
        }
        false
    }

    pub async fn init_linter(&self, options: &Options) {
        *self.options.lock().await = options.clone();
        *self.server_linter.write().await = Some(ServerLinter::new(&self.root_uri, options));
    }

    pub async fn needs_init_linter(&self) -> bool {
        self.server_linter.read().await.is_none()
    }

    pub fn remove_diagnostics(&self, uri: &Uri) {
        self.diagnostics_report_map.pin().remove(&uri.to_string());
    }

    async fn refresh_server_linter(&self) {
        let options = self.options.lock().await;
        let server_linter = ServerLinter::new(&self.root_uri, &options);

        *self.server_linter.write().await = Some(server_linter);
    }

    fn needs_linter_restart(old_options: &Options, new_options: &Options) -> bool {
        old_options.config_path != new_options.config_path
            || old_options.use_nested_configs() != new_options.use_nested_configs()
            || old_options.fix_kind() != new_options.fix_kind()
    }

    pub async fn should_lint_on_run_type(&self, current_run: Run) -> bool {
        let run_level = { self.options.lock().await.run };

        run_level == current_run
    }

    pub async fn lint_file(
        &self,
        uri: &Uri,
        content: Option<String>,
    ) -> Option<Vec<DiagnosticReport>> {
        let diagnostics = self.lint_file_internal(uri, content).await;

        if let Some(diagnostics) = &diagnostics {
            self.update_diagnostics(uri, diagnostics);
        }

        diagnostics
    }

    async fn lint_file_internal(
        &self,
        uri: &Uri,
        content: Option<String>,
    ) -> Option<Vec<DiagnosticReport>> {
        let Some(server_linter) = &*self.server_linter.read().await else {
            return None;
        };

        server_linter.run_single(uri, content)
    }

    fn update_diagnostics(&self, uri: &Uri, diagnostics: &[DiagnosticReport]) {
        self.diagnostics_report_map.pin().insert(uri.to_string(), diagnostics.to_owned());
    }

    async fn revalidate_diagnostics(&self) -> ConcurrentHashMap<String, Vec<DiagnosticReport>> {
        let diagnostics_map = ConcurrentHashMap::with_capacity_and_hasher(
            self.diagnostics_report_map.len(),
            FxBuildHasher,
        );
        let server_linter = self.server_linter.read().await;
        let Some(server_linter) = &*server_linter else {
            debug!("no server_linter initialized in the worker");
            return diagnostics_map;
        };

        for uri in self.diagnostics_report_map.pin_owned().keys() {
            if let Some(diagnostics) = server_linter.run_single(&Uri::from_str(uri).unwrap(), None)
            {
                self.diagnostics_report_map.pin().insert(uri.clone(), diagnostics.clone());
                diagnostics_map.pin().insert(uri.clone(), diagnostics);
            } else {
                self.diagnostics_report_map.pin().remove(uri);
            }
        }

        diagnostics_map
    }

    pub fn get_clear_diagnostics(&self) -> Vec<(String, Vec<Diagnostic>)> {
        self.diagnostics_report_map
            .pin()
            .keys()
            .map(|uri| (uri.clone(), vec![]))
            .collect::<Vec<_>>()
    }

    pub async fn get_code_actions_or_commands(
        &self,
        uri: &Uri,
        range: &Range,
        is_source_fix_all_oxc: bool,
    ) -> Vec<CodeActionOrCommand> {
        let report_map_ref = self.diagnostics_report_map.pin_owned();
        let value = match report_map_ref.get(&uri.to_string()) {
            Some(value) => value,
            // code actions / commands can be requested without opening the file
            // we just internally lint and provide the code actions / commands without refreshing the diagnostic map.
            None => &self.lint_file_internal(uri, None).await.unwrap_or_default(),
        };

        if value.is_empty() {
            return vec![];
        }

        let reports = value
            .iter()
            .filter(|r| r.diagnostic.range == *range || range_overlaps(*range, r.diagnostic.range));

        if is_source_fix_all_oxc {
            return apply_all_fix_code_action(reports, uri).map_or(vec![], |code_actions| {
                vec![CodeActionOrCommand::CodeAction(code_actions)]
            });
        }

        let mut code_actions_vec: Vec<CodeActionOrCommand> = vec![];

        for report in reports {
            if let Some(fix_action) = apply_fix_code_action(report, uri) {
                code_actions_vec.push(CodeActionOrCommand::CodeAction(fix_action));
            }

            code_actions_vec
                .push(CodeActionOrCommand::CodeAction(ignore_this_line_code_action(report, uri)));

            code_actions_vec
                .push(CodeActionOrCommand::CodeAction(ignore_this_rule_code_action(report, uri)));
        }

        code_actions_vec
    }

    pub async fn get_diagnostic_text_edits(&self, uri: &Uri) -> Vec<TextEdit> {
        let report_map_ref = self.diagnostics_report_map.pin_owned();
        let value = match report_map_ref.get(&uri.to_string()) {
            Some(value) => value,
            // code actions / commands can be requested without opening the file
            // we just internally lint and provide the code actions / commands without refreshing the diagnostic map.
            None => &self.lint_file_internal(uri, None).await.unwrap_or_default(),
        };

        if value.is_empty() {
            return vec![];
        }

        let mut text_edits = vec![];

        for report in value {
            if let Some(fixed_content) = &report.fixed_content {
                text_edits.push(TextEdit {
                    range: fixed_content.range,
                    new_text: fixed_content.code.clone(),
                });
            }
        }

        text_edits
    }

    pub async fn did_change_watched_files(&self, _file_event: &FileEvent) -> ClientNotifications {
        self.refresh_server_linter().await;
        ClientNotifications {
            diagnostics: Some(self.revalidate_diagnostics().await),
            add_file_watchers: None,
            remove_file_watchers: None,
        }
    }

    pub async fn did_change_configuration(&self, changed_options: &Options) -> ClientNotifications {
        let current_option = &self.options.lock().await.clone();

        debug!(
            "
        configuration changed:
        incoming: {changed_options:?}
        current: {current_option:?}
        "
        );

        *self.options.lock().await = changed_options.clone();

        if Self::needs_linter_restart(current_option, changed_options) {
            self.refresh_server_linter().await;
            return ClientNotifications {
                diagnostics: Some(self.revalidate_diagnostics().await),
                add_file_watchers: None,
                remove_file_watchers: None,
            };
        }

        ClientNotifications {
            diagnostics: None,
            add_file_watchers: None,
            remove_file_watchers: None,
        }
    }
}

fn range_overlaps(a: Range, b: Range) -> bool {
    a.start <= b.end && a.end >= b.start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_root_uri() {
        let worker = WorkspaceWorker::new(Uri::from_str("file:///root/").unwrap());

        assert_eq!(worker.get_root_uri(), &Uri::from_str("file:///root/").unwrap());
    }

    #[test]
    fn test_is_responsible() {
        let worker = WorkspaceWorker::new(Uri::from_str("file:///path/to/root").unwrap());

        assert!(
            worker.is_responsible_for_uri(&Uri::from_str("file:///path/to/root/file.js").unwrap())
        );
        assert!(worker.is_responsible_for_uri(
            &Uri::from_str("file:///path/to/root/folder/file.js").unwrap()
        ));
        assert!(
            !worker
                .is_responsible_for_uri(&Uri::from_str("file:///path/to/other/file.js").unwrap())
        );
    }
}
