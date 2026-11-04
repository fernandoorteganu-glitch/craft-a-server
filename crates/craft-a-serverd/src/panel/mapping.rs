//! Turns lifecycle errors into friendly, safe text and panel errors.
//!
//! Only fixed strings (from `strings.rs`) and the service's own validation
//! messages reach the screen; causes with paths, SQL or podman output are
//! logged by the caller and never shown.

use super::error::PanelError;
use super::strings as s;
use crate::lifecycle::ServiceError;
use crate::tunnel::TunnelError;

/// The sentence shown to the person for a failed action.
pub fn service_error_text(e: &ServiceError) -> String {
    match e {
        ServiceError::InvalidInput(message) => capitalize(message),
        ServiceError::NotFound(_) => s::ERR_NOT_FOUND.to_owned(),
        ServiceError::WrongState { .. } => s::ERR_SERVER_STATE.to_owned(),
        ServiceError::EulaNotAccepted => s::ERR_EULA.to_owned(),
        ServiceError::StartFailed(_) => s::ERR_START_FAILED.to_owned(),
        ServiceError::DiskLow => s::ERR_DISK_LOW.to_owned(),
        ServiceError::Memory(_) => s::ERR_MEMORY.to_owned(),
        // Written for people and free of paths; may hold mod names, which
        // the templates escape.
        ServiceError::Mods(m) => capitalize(&crate::mods::logic::clean_text(&m.to_string(), 500)),
        // Fixed sentences without paths.
        ServiceError::Tunnel(TunnelError::Db(_) | TunnelError::Io(_)) => {
            s::ERR_ACTION_FAILED.to_owned()
        }
        ServiceError::Tunnel(t) => capitalize(&t.to_string()),
        ServiceError::WorldImport(m) => crate::mods::logic::clean_text(m, 600),
        ServiceError::Db(_)
        | ServiceError::Runtime(_)
        | ServiceError::Backup(_)
        | ServiceError::Io { .. } => s::ERR_ACTION_FAILED.to_owned(),
    }
}

/// The panel error for a failed synchronous action (logs internal causes).
pub fn service_error(e: &ServiceError) -> PanelError {
    let text = service_error_text(e);
    match e {
        ServiceError::InvalidInput(_) | ServiceError::EulaNotAccepted => PanelError::Invalid(text),
        ServiceError::NotFound(_) => PanelError::NotFound,
        ServiceError::Tunnel(TunnelError::Db(_) | TunnelError::Io(_)) => PanelError::internal(e),
        ServiceError::Tunnel(TunnelError::NotConfirmed) => PanelError::Invalid(text),
        ServiceError::Tunnel(_) => PanelError::Conflict(text),
        ServiceError::WrongState { .. }
        | ServiceError::DiskLow
        | ServiceError::Memory(_)
        | ServiceError::Mods(_)
        | ServiceError::WorldImport(_)
        | ServiceError::StartFailed(_) => PanelError::Conflict(text),
        ServiceError::Db(_)
        | ServiceError::Runtime(_)
        | ServiceError::Backup(_)
        | ServiceError::Io { .. } => PanelError::internal(e),
    }
}

fn capitalize(message: &str) -> String {
    let mut chars = message.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_causes_are_not_shown() {
        let e = ServiceError::Io {
            context: "secret path /var/lib/x",
            source: std::io::Error::other("boom"),
        };
        let text = service_error_text(&e);
        assert_eq!(text, s::ERR_ACTION_FAILED);
        assert!(!text.contains("secret"));
        assert_eq!(service_error(&e), PanelError::Internal);
    }

    #[test]
    fn validation_messages_pass_through_capitalized() {
        let e = ServiceError::InvalidInput("invalid Minecraft version".into());
        assert_eq!(service_error_text(&e), "Invalid Minecraft version");
        assert!(matches!(service_error(&e), PanelError::Invalid(_)));
    }

    #[test]
    fn state_errors_are_conflicts() {
        let e = ServiceError::WrongState {
            id: "s1".into(),
            state: "running".into(),
        };
        assert!(matches!(service_error(&e), PanelError::Conflict(_)));
    }
}
