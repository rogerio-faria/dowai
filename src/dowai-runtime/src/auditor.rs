//! Emission of AUDIT events for one run (plan 12.2, 12.3 item 9; P21).
//!
//! The AUDIT trail is written unconditionally: no project or log configuration can
//! turn it off. A failed write never changes the run outcome; it is remembered in
//! `failed` and reported once as a sanitized warning by the caller.

use dowai_core::{default_audit_log_path, AuditEvent, AuditLog, DowaiError};

pub(crate) struct Auditor {
    log: Option<AuditLog>,
    run_id: String,
    root_invocation_id: String,
    started_ms: i64,
    started_emitted: bool,
    pub(crate) failed: bool,
    pub(crate) session_id: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) config_hash: Option<String>,
    pub(crate) plan_hash: Option<String>,
    pub(crate) profile: Option<String>,
}

impl Auditor {
    pub(crate) fn new(
        run_id: &str,
        root_invocation_id: &str,
        started_ms: i64,
        session_id: Option<String>,
        project: Option<String>,
    ) -> Self {
        let log = default_audit_log_path().map(AuditLog::for_user_state);
        Self {
            failed: log.is_none(),
            log,
            run_id: run_id.to_owned(),
            root_invocation_id: root_invocation_id.to_owned(),
            started_ms,
            started_emitted: false,
            session_id,
            project,
            mode: None,
            config_hash: None,
            plan_hash: None,
            profile: None,
        }
    }

    fn event(&self, name: &str) -> AuditEvent {
        let mut event = AuditEvent::new(name, &self.run_id);
        event.session_id = self.session_id.clone();
        event.project = self.project.clone();
        event.mode = self.mode.clone();
        event.config_hash = self.config_hash.clone();
        event.plan_hash = self.plan_hash.clone();
        event.source_profile = self.profile.clone();
        event.target_profile = self.profile.clone();
        event
    }

    fn emit(&mut self, event: &AuditEvent) {
        let written = match &self.log {
            Some(log) => log.append(event).is_ok(),
            None => false,
        };
        if !written {
            self.failed = true;
        }
    }

    /// Emits `run.started` once, stamped with the real start time.
    pub(crate) fn ensure_started(&mut self) {
        if self.started_emitted {
            return;
        }
        self.started_emitted = true;
        let mut event = self.event("run.started").at_ms(self.started_ms);
        event.invocation_id = Some(self.root_invocation_id.clone());
        event.attempt_id = event.invocation_id.clone();
        self.emit(&event);
    }

    pub(crate) fn node_started(
        &mut self,
        invocation_id: &str,
        node_id: &str,
        script_hash: Option<String>,
    ) {
        let mut event = self.event("node.started");
        event.invocation_id = Some(invocation_id.to_owned());
        event.attempt_id = Some(invocation_id.to_owned());
        event.parent_invocation_id = Some(self.root_invocation_id.clone());
        event.node_id = Some(node_id.to_owned());
        event.call_type = Some("static".to_owned());
        event.script_hash = script_hash;
        self.emit(&event);
    }

    pub(crate) fn node_finished(
        &mut self,
        invocation_id: &str,
        node_id: &str,
        error: Option<&DowaiError>,
    ) {
        let mut event = self.event("node.finished");
        event.invocation_id = Some(invocation_id.to_owned());
        event.attempt_id = Some(invocation_id.to_owned());
        event.parent_invocation_id = Some(self.root_invocation_id.clone());
        event.node_id = Some(node_id.to_owned());
        event.call_type = Some("static".to_owned());
        event.result = Some(
            if error.is_some() {
                "FAILED"
            } else {
                "SUCCEEDED"
            }
            .to_owned(),
        );
        event.error_code = error.map(|error| error.code().to_owned());
        self.emit(&event);
    }

    pub(crate) fn variables_published(&mut self, names: Vec<String>) {
        let mut event = self.event("variables.published");
        event.invocation_id = Some(self.root_invocation_id.clone());
        event.variables = Some(names);
        self.emit(&event);
    }

    /// A SQL node replaced or created a relation in the target database.
    pub(crate) fn external_write(&mut self, invocation_id: &str, node_id: &str, relation: &str) {
        let mut event = self.event("external.write");
        event.invocation_id = Some(invocation_id.to_owned());
        event.attempt_id = Some(invocation_id.to_owned());
        event.node_id = Some(node_id.to_owned());
        event.relation = Some(relation.to_owned());
        event.result = Some("SUCCEEDED".to_owned());
        self.emit(&event);
    }

    pub(crate) fn python_invocation(
        &mut self,
        invocation_id: &str,
        parent_invocation_id: &str,
        call_type: &str,
        target: &str,
        status: &str,
        error_code: Option<&str>,
    ) {
        let mut event = self.event("python.invocation");
        event.invocation_id = Some(invocation_id.to_owned());
        event.attempt_id = Some(invocation_id.to_owned());
        event.parent_invocation_id = Some(parent_invocation_id.to_owned());
        event.call_type = Some(call_type.to_owned());
        event.node_id = Some(target.to_owned());
        event.result = Some(status.to_owned());
        event.error_code = error_code.map(str::to_owned);
        self.emit(&event);
    }

    pub(crate) fn policy_denied(&mut self, code: &str) {
        let mut event = self.event("policy.denied");
        event.invocation_id = Some(self.root_invocation_id.clone());
        event.result = Some("DENIED".to_owned());
        event.error_code = Some(code.to_owned());
        self.emit(&event);
    }

    pub(crate) fn run_finished(&mut self, error: Option<&DowaiError>) {
        self.ensure_started();
        let mut event = self.event("run.finished");
        event.invocation_id = Some(self.root_invocation_id.clone());
        event.attempt_id = event.invocation_id.clone();
        event.result = Some(
            if error.is_some() {
                "FAILED"
            } else {
                "SUCCEEDED"
            }
            .to_owned(),
        );
        event.error_code = error.map(|error| error.code().to_owned());
        self.emit(&event);
    }
}
