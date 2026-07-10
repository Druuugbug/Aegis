use std::collections::HashSet;
use std::sync::Arc;

/// Object-safe plugin trait for hooking into agent lifecycle events.
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;

    fn on_user_message(&self, _msg: &str) -> Option<String> {
        None
    }
    fn on_assistant_message(&self, _msg: &str) {}
    fn on_tool_call(&self, _tool: &str, _args: &str) {}
    fn on_tool_result(&self, _tool: &str, _result: &str) {}
    fn on_session_start(&self, _session_id: &str) {}
    fn on_session_end(&self, _session_id: &str) {}
}

/// Registry that manages plugins and dispatches lifecycle hooks.
pub struct PluginRegistry {
    plugins: Vec<Arc<dyn Plugin>>,
    enabled: HashSet<String>,
}

impl PluginRegistry {
    /// Create an empty plugin registry.
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            enabled: HashSet::new(),
        }
    }

    /// Register a plugin (duplicates by name are silently ignored).
    pub fn register(&mut self, plugin: Arc<dyn Plugin>) {
        let name = plugin.name().to_string();
        if !self.plugins.iter().any(|p| p.name() == name) {
            self.enabled.insert(name);
            self.plugins.push(plugin);
        }
    }

    /// Remove a plugin by name. Returns true if it was found and removed.
    pub fn unregister(&mut self, name: &str) -> bool {
        if let Some(pos) = self.plugins.iter().position(|p| p.name() == name) {
            self.plugins.remove(pos);
            self.enabled.remove(name);
            true
        } else {
            false
        }
    }

    /// Look up a plugin by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Plugin>> {
        self.plugins.iter().find(|p| p.name() == name).cloned()
    }

    /// List the names of all registered plugins.
    pub fn list(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name()).collect()
    }

    /// Enable a registered plugin by name. Returns false if not found.
    pub fn enable(&mut self, name: &str) -> bool {
        if self.plugins.iter().any(|p| p.name() == name) {
            self.enabled.insert(name.to_string());
            true
        } else {
            false
        }
    }

    /// Disable a plugin by name, preventing its hooks from firing.
    pub fn disable(&mut self, name: &str) -> bool {
        self.enabled.remove(name)
    }

    /// Check whether a plugin is currently enabled.
    pub fn is_enabled(&self, name: &str) -> bool {
        self.enabled.contains(name)
    }

    // ── Fire methods ──

    /// Fire the on_user_message hook on all enabled plugins.
    pub fn fire_user_message(&self, msg: &str) -> Vec<String> {
        self.plugins
            .iter()
            .filter(|p| self.enabled.contains(p.name()))
            .filter_map(|p| p.on_user_message(msg))
            .collect()
    }

    /// Fire the on_assistant_message hook on all enabled plugins.
    pub fn fire_assistant_message(&self, msg: &str) {
        for p in &self.plugins {
            if self.enabled.contains(p.name()) {
                p.on_assistant_message(msg);
            }
        }
    }

    /// Fire the on_tool_call hook on all enabled plugins.
    pub fn fire_tool_call(&self, tool: &str, args: &str) {
        for p in &self.plugins {
            if self.enabled.contains(p.name()) {
                p.on_tool_call(tool, args);
            }
        }
    }

    /// Fire the on_tool_result hook on all enabled plugins.
    pub fn fire_tool_result(&self, tool: &str, result: &str) {
        for p in &self.plugins {
            if self.enabled.contains(p.name()) {
                p.on_tool_result(tool, result);
            }
        }
    }

    /// Fire the on_session_start hook on all enabled plugins.
    pub fn fire_session_start(&self, session_id: &str) {
        for p in &self.plugins {
            if self.enabled.contains(p.name()) {
                p.on_session_start(session_id);
            }
        }
    }

    /// Fire the on_session_end hook on all enabled plugins.
    pub fn fire_session_end(&self, session_id: &str) {
        for p in &self.plugins {
            if self.enabled.contains(p.name()) {
                p.on_session_end(session_id);
            }
        }
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Built-in LogPlugin ──

pub struct LogPlugin;

impl Plugin for LogPlugin {
    fn name(&self) -> &str {
        "log"
    }

    fn on_user_message(&self, msg: &str) -> Option<String> {
        tracing::debug!(target: "aegis::plugin::log", user_message = msg);
        None
    }

    fn on_assistant_message(&self, msg: &str) {
        tracing::debug!(target: "aegis::plugin::log", assistant_message = msg);
    }

    fn on_tool_call(&self, tool: &str, args: &str) {
        tracing::debug!(target: "aegis::plugin::log", tool, args);
    }

    fn on_tool_result(&self, tool: &str, result: &str) {
        tracing::debug!(target: "aegis::plugin::log", tool, result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPlugin {
        name: String,
    }

    impl Plugin for TestPlugin {
        fn name(&self) -> &str {
            &self.name
        }
    }

    #[test]
    fn test_register_unregister() {
        let mut reg = PluginRegistry::new();
        let p = Arc::new(TestPlugin {
            name: "test".to_string(),
        });
        reg.register(p);
        assert_eq!(reg.list(), vec!["test"]);
        assert!(reg.is_enabled("test"));

        assert!(reg.unregister("test"));
        assert!(reg.list().is_empty());
        assert!(!reg.is_enabled("test"));
    }

    #[test]
    fn test_enable_disable() {
        let mut reg = PluginRegistry::new();
        let p = Arc::new(TestPlugin {
            name: "test".to_string(),
        });
        reg.register(p);

        assert!(reg.disable("test"));
        assert!(!reg.is_enabled("test"));

        assert!(reg.enable("test"));
        assert!(reg.is_enabled("test"));
    }

    #[test]
    fn test_fire_disabled_plugin_skipped() {
        struct CollectPlugin {
            name: String,
            collected: std::sync::Mutex<Vec<String>>,
        }

        impl Plugin for CollectPlugin {
            fn name(&self) -> &str {
                &self.name
            }
            fn on_user_message(&self, msg: &str) -> Option<String> {
                self.collected.lock().unwrap().push(msg.to_string());
                Some(format!("modified:{msg}"))
            }
        }

        let mut reg = PluginRegistry::new();
        let p = Arc::new(CollectPlugin {
            name: "collect".to_string(),
            collected: std::sync::Mutex::new(Vec::new()),
        });
        reg.register(p.clone());

        // Enabled → collects
        let results = reg.fire_user_message("hello");
        assert_eq!(results, vec!["modified:hello"]);

        // Disable → skipped
        reg.disable("collect");
        let results = reg.fire_user_message("world");
        assert!(results.is_empty());
    }

    #[test]
    fn test_duplicate_register_ignored() {
        let mut reg = PluginRegistry::new();
        let p1 = Arc::new(TestPlugin {
            name: "dup".to_string(),
        });
        let p2 = Arc::new(TestPlugin {
            name: "dup".to_string(),
        });
        reg.register(p1);
        reg.register(p2);
        assert_eq!(reg.list().len(), 1);
    }
}
