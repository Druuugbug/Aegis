/// Skill requirements checker: validates binaries and env vars before loading a skill.

#[derive(Debug, Clone)]
/// Whether a skill's requirements are satisfied.
pub enum SkillAvailability {
    Ready,
    Unavailable(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
/// Declares binaries, env vars, and version needed to load a skill.
pub struct SkillRequirements {
    pub binaries: Vec<String>,
    pub env_vars: Vec<String>,
    pub min_version: Option<String>,
}

impl SkillRequirements {
    /// Create empty requirements (everything satisfied).
    pub fn new() -> Self {
        Self {
            binaries: Vec::new(),
            env_vars: Vec::new(),
            min_version: None,
        }
    }

    /// Verify all required binaries and env vars are present.
    pub fn check(&self) -> SkillAvailability {
        for bin in &self.binaries {
            if std::process::Command::new("which")
                .arg(bin)
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(true)
            {
                return SkillAvailability::Unavailable(format!("missing binary: {}", bin));
            }
        }
        for var in &self.env_vars {
            if std::env::var(var).is_err() {
                return SkillAvailability::Unavailable(format!("missing env var: {}", var));
            }
        }
        SkillAvailability::Ready
    }
}

impl Default for SkillRequirements {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_requirements_empty_is_ready() {
        let reqs = SkillRequirements::new();
        assert!(matches!(reqs.check(), SkillAvailability::Ready));
    }

    #[test]
    fn skill_requirements_missing_binary() {
        let reqs = SkillRequirements {
            binaries: vec!["nonexistent_binary_xyz_12345".into()],
            env_vars: vec![],
            min_version: None,
        };
        match reqs.check() {
            SkillAvailability::Unavailable(msg) => assert!(msg.contains("missing binary")),
            _ => panic!("expected Unavailable"),
        }
    }

    #[test]
    fn skill_requirements_present_binary() {
        let reqs = SkillRequirements {
            binaries: vec!["ls".into()],
            env_vars: vec![],
            min_version: None,
        };
        assert!(matches!(reqs.check(), SkillAvailability::Ready));
    }

    #[test]
    fn skill_requirements_missing_env_var() {
        let reqs = SkillRequirements {
            binaries: vec![],
            env_vars: vec!["TOTALLY_FAKE_ENV_VAR_12345678".into()],
            min_version: None,
        };
        match reqs.check() {
            SkillAvailability::Unavailable(msg) => assert!(msg.contains("missing env var")),
            _ => panic!("expected Unavailable"),
        }
    }

    #[test]
    fn skill_requirements_present_env_var() {
        // HOME should exist everywhere
        let reqs = SkillRequirements {
            binaries: vec![],
            env_vars: vec!["HOME".into()],
            min_version: None,
        };
        assert!(matches!(reqs.check(), SkillAvailability::Ready));
    }

    #[test]
    fn skill_requirements_combined() {
        let reqs = SkillRequirements {
            binaries: vec!["ls".into()],
            env_vars: vec!["HOME".into()],
            min_version: Some("1.0.0".into()),
        };
        assert!(matches!(reqs.check(), SkillAvailability::Ready));
    }

    #[test]
    fn skill_requirements_default() {
        let reqs = SkillRequirements::default();
        assert!(matches!(reqs.check(), SkillAvailability::Ready));
    }

    #[test]
    fn skill_availability_debug() {
        let avail = SkillAvailability::Ready;
        assert!(format!("{:?}", avail).contains("Ready"));

        let unavail = SkillAvailability::Unavailable("test reason".into());
        assert!(format!("{:?}", unavail).contains("test reason"));
    }
}
