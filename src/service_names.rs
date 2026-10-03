//! Windows service names and the SCM-independent decision logic around them.
//!
//! Kept cross-platform (and free of SCM calls) so the migration decisions are
//! unit-tested with a mock; `service.rs` and `update_transaction.rs` provide the
//! real SCM-backed implementations of [`ServiceControl`].

/// Current Windows service name (SCM namespace). The autostart registry Run
/// value is also called `OpenVibeSttServer`, but that is a separate namespace
/// (HKCU Run value vs. SCM service), so the two never collide.
pub const SERVICE_NAME: &str = "OpenVibeSttServer";
/// Service name used up to 0.3.1 (the working name `stt-server-next`).
/// LEGACY MIGRATION ONLY: `service install` replaces it, `service uninstall`
/// and the self-updater still recognise it.
pub const LEGACY_SERVICE_NAME: &str = "OpenVibeSttNext";
pub const DISPLAY_NAME: &str = "STT Server";

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The minimal service-manager surface the decisions need.
pub trait ServiceControl {
    fn exists(&self, name: &str) -> Result<bool>;
    /// Stops the service (waiting until stopped) and deletes it.
    fn stop_and_delete(&self, name: &str) -> Result<()>;
}

/// Legacy migration: if a legacy-named service is registered, stop and delete
/// it so the new one can be installed. Returns whether one was removed.
pub fn migrate_legacy(control: &dyn ServiceControl) -> Result<bool> {
    if control.exists(LEGACY_SERVICE_NAME)? {
        control.stop_and_delete(LEGACY_SERVICE_NAME)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// `service uninstall` removes whichever of the current and legacy services
/// exist. Returns the names removed (empty if neither was registered).
pub fn remove_all(control: &dyn ServiceControl) -> Result<Vec<&'static str>> {
    let mut removed = Vec::new();
    for name in [SERVICE_NAME, LEGACY_SERVICE_NAME] {
        if control.exists(name)? {
            control.stop_and_delete(name)?;
            removed.push(name);
        }
    }
    Ok(removed)
}

/// The name SCM start/stop should use for an installed service: the current
/// name when registered, else a not-yet-migrated legacy install, else the
/// current name (so the failure message names the current service).
pub fn active_name(control: &dyn ServiceControl) -> &'static str {
    if control.exists(SERVICE_NAME).unwrap_or(false) {
        SERVICE_NAME
    } else if control.exists(LEGACY_SERVICE_NAME).unwrap_or(false) {
        LEGACY_SERVICE_NAME
    } else {
        SERVICE_NAME
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Mock {
        present: RefCell<Vec<String>>,
        deleted: RefCell<Vec<String>>,
    }
    impl Mock {
        fn with(names: &[&str]) -> Self {
            Mock {
                present: RefCell::new(names.iter().map(|n| n.to_string()).collect()),
                deleted: RefCell::new(vec![]),
            }
        }
    }
    impl ServiceControl for Mock {
        fn exists(&self, name: &str) -> Result<bool> {
            Ok(self.present.borrow().iter().any(|n| n == name))
        }
        fn stop_and_delete(&self, name: &str) -> Result<()> {
            self.present.borrow_mut().retain(|n| n != name);
            self.deleted.borrow_mut().push(name.to_owned());
            Ok(())
        }
    }

    #[test]
    fn names_are_distinct_and_display_name_has_no_next() {
        assert_ne!(SERVICE_NAME, LEGACY_SERVICE_NAME);
        assert!(!SERVICE_NAME.contains("Next"));
        assert!(!DISPLAY_NAME.contains("Next"));
    }

    #[test]
    fn migrate_removes_legacy_service_only() {
        let mock = Mock::with(&[LEGACY_SERVICE_NAME]);
        assert!(migrate_legacy(&mock).unwrap());
        assert_eq!(*mock.deleted.borrow(), vec![LEGACY_SERVICE_NAME]);
    }

    #[test]
    fn migrate_is_a_no_op_on_a_fresh_or_already_migrated_machine() {
        let mock = Mock::with(&[SERVICE_NAME]);
        assert!(!migrate_legacy(&mock).unwrap());
        assert!(mock.deleted.borrow().is_empty());
        assert!(!migrate_legacy(&Mock::with(&[])).unwrap());
    }

    #[test]
    fn uninstall_removes_either_or_both_names() {
        let mock = Mock::with(&[SERVICE_NAME, LEGACY_SERVICE_NAME]);
        assert_eq!(
            remove_all(&mock).unwrap(),
            vec![SERVICE_NAME, LEGACY_SERVICE_NAME]
        );
        assert_eq!(
            remove_all(&Mock::with(&[LEGACY_SERVICE_NAME])).unwrap(),
            vec![LEGACY_SERVICE_NAME]
        );
        assert_eq!(
            remove_all(&Mock::with(&[SERVICE_NAME])).unwrap(),
            vec![SERVICE_NAME]
        );
        assert!(remove_all(&Mock::with(&[])).unwrap().is_empty());
    }

    #[test]
    fn active_name_prefers_current_then_legacy_then_current() {
        assert_eq!(active_name(&Mock::with(&[SERVICE_NAME])), SERVICE_NAME);
        assert_eq!(
            active_name(&Mock::with(&[LEGACY_SERVICE_NAME])),
            LEGACY_SERVICE_NAME
        );
        assert_eq!(
            active_name(&Mock::with(&[SERVICE_NAME, LEGACY_SERVICE_NAME])),
            SERVICE_NAME
        );
        assert_eq!(active_name(&Mock::with(&[])), SERVICE_NAME);
    }
}
