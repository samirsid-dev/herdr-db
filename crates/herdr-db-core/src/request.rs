//! Requests passed from the pane that triggers an action to the pane that
//! opens. Primary channel: the `HERDR_DB_REQUEST` environment variable given
//! to `plugin.pane.open`. Fallback: a file queue in the plugin state dir, for
//! hosts that would not forward the environment.

use crate::model::ObjectRef;
use serde::{Deserialize, Serialize};

pub const ENV_VAR: &str = "HERDR_DB_REQUEST";
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    EditData,
    GoToDdl,
    QuickDoc,
    Console,
}

impl Action {
    /// Manifest pane entrypoint serving this action.
    pub fn entrypoint(self) -> &'static str {
        match self {
            Action::EditData => "grid",
            Action::GoToDdl => "ddl",
            Action::QuickDoc => "quickdoc",
            Action::Console => "console",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRequest {
    pub v: u32,
    pub action: Action,
    /// Target source. Implied by `object` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<ObjectRef>,
    /// Column inside `object` (Quick Documentation of a column).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// Default schema (console).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Initial `WHERE` clause (foreign key navigation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Name of the queue file carrying the same request, removed by the
    /// receiving pane so no other pane claims it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl PaneRequest {
    pub fn new(action: Action) -> Self {
        Self { v: VERSION, action, source: None, object: None, column: None, schema: None, filter: None, token: None }
    }

    pub fn for_object(action: Action, object: ObjectRef) -> Self {
        Self { object: Some(object), ..Self::new(action) }
    }

    pub fn source_id(&self) -> Option<&str> {
        self.object.as_ref().map(|o| o.source.as_str()).or(self.source.as_deref())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("request serializes")
    }

    /// Rejects requests from a future, incompatible binary.
    pub fn from_json(text: &str) -> Option<PaneRequest> {
        serde_json::from_str::<PaneRequest>(text).ok().filter(|r| r.v == VERSION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_example_parses() {
        let text = r#"{ "v": 1, "action": "edit_data",
            "object": { "source": "db_prod", "schema": "public", "name": "users" } }"#;
        let request = PaneRequest::from_json(text).unwrap();
        assert_eq!(request.action, Action::EditData);
        assert_eq!(request.source_id(), Some("db_prod"));
        assert_eq!(request.action.entrypoint(), "grid");
    }

    #[test]
    fn roundtrip_and_version_check() {
        let mut request = PaneRequest::new(Action::Console);
        request.source = Some("local".into());
        request.schema = Some("public".into());
        let json = request.to_json();
        assert_eq!(json, r#"{"v":1,"action":"console","source":"local","schema":"public"}"#);
        assert_eq!(PaneRequest::from_json(&json), Some(request));
        assert_eq!(PaneRequest::from_json(r#"{"v":2,"action":"console"}"#), None);
        assert_eq!(PaneRequest::from_json("garbage"), None);
    }
}
