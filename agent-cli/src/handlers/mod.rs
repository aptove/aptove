pub mod jobs;
pub mod session;
pub mod triggers;

/// Helper: get the scheduler store or return a JSON-RPC error.
#[macro_export]
macro_rules! require_scheduler_store {
    ($state:expr, $id:expr) => {
        match $state.scheduler_store.as_ref() {
            Some(s) => s.clone(),
            None => {
                return $state
                    .sender
                    .send_error(
                        $id,
                        agent_core::transport::error_codes::INTERNAL_ERROR,
                        "scheduler store not configured".to_string(),
                    )
                    .await;
            }
        }
    };
}

/// Helper: get the trigger store or return a JSON-RPC error.
#[macro_export]
macro_rules! require_trigger_store {
    ($state:expr, $id:expr) => {
        match $state.trigger_store.as_ref() {
            Some(s) => s.clone(),
            None => {
                return $state
                    .sender
                    .send_error(
                        $id,
                        agent_core::transport::error_codes::INTERNAL_ERROR,
                        "trigger store not configured".to_string(),
                    )
                    .await;
            }
        }
    };
}

/// Helper: extract a required string field from params or return a JSON-RPC error.
#[macro_export]
macro_rules! require_param_str {
    ($params:expr, $field:expr, $id:expr, $state:expr) => {
        match $params
            .as_ref()
            .and_then(|p| p.get($field))
            .and_then(|v| v.as_str())
        {
            Some(v) => v.to_string(),
            None => {
                return $state
                    .sender
                    .send_error(
                        $id,
                        agent_core::transport::error_codes::INVALID_PARAMS,
                        format!("missing required field: {}", $field),
                    )
                    .await;
            }
        }
    };
}
