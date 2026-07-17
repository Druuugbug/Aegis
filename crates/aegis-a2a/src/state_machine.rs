pub use crate::types::TaskState as TaskStatus;

/// Validates whether a task status transition is allowed.
pub fn validate_transition(from: &TaskStatus, to: &TaskStatus) -> bool {
    use TaskStatus::*;
    matches!(
        (from, to),
        (Submitted, Working)
            | (Submitted, Canceled)
            | (Submitted, Rejected)
            | (Submitted, AuthRequired)
            | (Working, Completed)
            | (Working, Failed)
            | (Working, Canceled)
            | (Working, InputRequired)
            | (Working, AuthRequired)
            | (InputRequired, Working)
            | (InputRequired, Canceled)
            | (InputRequired, Failed)
            | (AuthRequired, Working)
            | (AuthRequired, Canceled)
            | (AuthRequired, Failed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskStatus::*;

    #[test]
    fn test_valid_transitions() {
        assert!(validate_transition(&Submitted, &Working));
        assert!(validate_transition(&Submitted, &Canceled));
        assert!(validate_transition(&Submitted, &Rejected));
        assert!(validate_transition(&Working, &Completed));
        assert!(validate_transition(&Working, &Failed));
        assert!(validate_transition(&Working, &Canceled));
        assert!(validate_transition(&Working, &InputRequired));
        assert!(validate_transition(&InputRequired, &Working));
        assert!(validate_transition(&Submitted, &AuthRequired));
        assert!(validate_transition(&AuthRequired, &Working));
    }

    #[test]
    fn test_invalid_transitions() {
        assert!(!validate_transition(&Submitted, &Completed));
        assert!(!validate_transition(&Submitted, &Failed));
        assert!(!validate_transition(&Working, &Submitted));
        assert!(!validate_transition(&Working, &Rejected));
        assert!(!validate_transition(&Completed, &Working));
        assert!(!validate_transition(&Completed, &Failed));
        assert!(!validate_transition(&Failed, &Working));
        assert!(!validate_transition(&Canceled, &Working));
        assert!(!validate_transition(&Rejected, &Working));
    }

    #[test]
    fn test_terminal_states_cannot_transition() {
        let terminal = [Completed, Failed, Canceled, Rejected];
        let all_states = [Submitted, Working, Completed, Failed, Canceled, Rejected];
        for from in &terminal {
            for to in &all_states {
                assert!(
                    !validate_transition(from, to),
                    "{from:?} -> {to:?} should be invalid"
                );
            }
        }
    }
}
