use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::registry::{Tool, ToolContext};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Widget {
    pub id: String,
    #[serde(rename = "type")]
    pub widget_type: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub value: Value,
    #[serde(default = "default_max")]
    pub max: u64,
    #[serde(default)]
    pub content: String,
    #[serde(default = "default_color")]
    pub color: String,
}

fn default_max() -> u64 {
    100
}
fn default_color() -> String {
    "dim".to_string()
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WidgetFile {
    #[serde(default)]
    pub widgets: Vec<Widget>,
}

pub fn widgets_path() -> PathBuf {
    aegis_types::paths::config_dir().join("widgets.json")
}

pub fn load_widgets() -> Vec<Widget> {
    let path = widgets_path();
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<WidgetFile>(&s).ok())
        .map(|f| f.widgets)
        .unwrap_or_default()
}

fn save_widgets(widgets: &[Widget]) -> Result<()> {
    let path = widgets_path();
    let _ = std::fs::create_dir_all(path.parent().expect("widgets path has parent"));
    let file = WidgetFile {
        widgets: widgets.to_vec(),
    };
    let json = serde_json::to_string_pretty(&file)?;
    std::fs::write(&path, json)?;
    Ok(())
}

pub struct WidgetTool;

#[async_trait]
impl Tool for WidgetTool {
    fn name(&self) -> &str {
        "widget"
    }
    fn description(&self) -> &str {
        "Manage persistent widgets displayed below the input prompt. Use to show \
         the user ongoing status (git branch, test coverage, deploy state, etc.) \
         that persists across turns and sessions. Widget types: kv (label: value), \
         bar (label with progress), text (free-form line)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["set", "remove", "list", "clear"],
                    "description": "Action to perform"
                },
                "id": {
                    "type": "string",
                    "description": "Unique widget identifier (required for set/remove)"
                },
                "type": {
                    "type": "string",
                    "enum": ["kv", "bar", "text"],
                    "description": "Widget type (required for set)"
                },
                "label": {
                    "type": "string",
                    "description": "Display label (for kv/bar types)"
                },
                "value": {
                    "description": "Display value: string for kv, number for bar"
                },
                "max": {
                    "type": "integer",
                    "description": "Max value for bar type (default 100)"
                },
                "content": {
                    "type": "string",
                    "description": "Full text content (for text type)"
                },
                "color": {
                    "type": "string",
                    "enum": ["cyan", "green", "yellow", "red", "magenta", "dim", "white"],
                    "description": "Display color (default: dim)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("list");

        match action {
            "set" => {
                let id = match args["id"].as_str() {
                    Some(id) if !id.is_empty() => id.to_string(),
                    _ => return Ok("Error: 'id' is required for set action.".to_string()),
                };
                let widget_type = match args["type"].as_str() {
                    Some(t) if matches!(t, "kv" | "bar" | "text") => t.to_string(),
                    _ => return Ok("Error: 'type' must be one of: kv, bar, text.".to_string()),
                };

                let widget = Widget {
                    id: id.clone(),
                    widget_type,
                    label: args["label"].as_str().unwrap_or("").to_string(),
                    value: args["value"].clone(),
                    max: args["max"].as_u64().unwrap_or(100),
                    content: args["content"].as_str().unwrap_or("").to_string(),
                    color: args["color"].as_str().unwrap_or("dim").to_string(),
                };

                let mut widgets = load_widgets();
                if let Some(pos) = widgets.iter().position(|w| w.id == id) {
                    widgets[pos] = widget;
                } else {
                    widgets.push(widget);
                }
                save_widgets(&widgets)?;
                Ok(format!("Widget '{id}' set. ({} total)", widgets.len()))
            }
            "remove" => {
                let id = match args["id"].as_str() {
                    Some(id) if !id.is_empty() => id,
                    _ => return Ok("Error: 'id' is required for remove action.".to_string()),
                };
                let mut widgets = load_widgets();
                let before = widgets.len();
                widgets.retain(|w| w.id != id);
                if widgets.len() == before {
                    return Ok(format!("No widget found with id '{id}'."));
                }
                save_widgets(&widgets)?;
                Ok(format!(
                    "Widget '{id}' removed. ({} remaining)",
                    widgets.len()
                ))
            }
            "list" => {
                let widgets = load_widgets();
                if widgets.is_empty() {
                    return Ok("No widgets configured.".to_string());
                }
                let mut out = String::new();
                for w in &widgets {
                    out.push_str(&format!(
                        "- [{}] type={}, label={:?}, value={}, color={}\n",
                        w.id, w.widget_type, w.label, w.value, w.color
                    ));
                }
                Ok(out)
            }
            "clear" => {
                save_widgets(&[])?;
                Ok("All widgets cleared.".to_string())
            }
            _ => Ok("Unknown action. Use: set, remove, list, clear.".to_string()),
        }
    }
}

/// Render widgets into displayable lines. Each line is prefixed with `  ┊ `.
/// Returns empty vec if no widgets are configured.
pub fn render_widget_lines(widgets: &[Widget]) -> Vec<String> {
    widgets
        .iter()
        .filter_map(|w| {
            let body = match w.widget_type.as_str() {
                "kv" => {
                    let val = match &w.value {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => n.to_string(),
                        Value::Bool(b) => b.to_string(),
                        _ => w.value.to_string(),
                    };
                    format!("{}: {}", w.label, val)
                }
                "bar" => {
                    let val = w.value.as_f64().unwrap_or(0.0) as u64;
                    let max = w.max.max(1);
                    let pct = (val * 100 / max).min(100);
                    let filled = (val * 10 / max).min(10) as usize;
                    let empty = 10 - filled;
                    format!(
                        "{}: {}% {}{}",
                        w.label,
                        pct,
                        "\u{25b0}".repeat(filled),
                        "\u{25b1}".repeat(empty),
                    )
                }
                "text" => w.content.clone(),
                _ => return None,
            };
            Some(format!("  \u{250a} {body}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_kv_widget() {
        let w = Widget {
            id: "branch".to_string(),
            widget_type: "kv".to_string(),
            label: "Branch".to_string(),
            value: Value::String("main".to_string()),
            max: 100,
            content: String::new(),
            color: "cyan".to_string(),
        };
        let lines = render_widget_lines(&[w]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Branch: main"));
    }

    #[test]
    fn test_render_bar_widget() {
        let w = Widget {
            id: "cov".to_string(),
            widget_type: "bar".to_string(),
            label: "Coverage".to_string(),
            value: json!(80),
            max: 100,
            content: String::new(),
            color: "green".to_string(),
        };
        let lines = render_widget_lines(&[w]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Coverage: 80%"));
        assert!(lines[0].contains("\u{25b0}"));
    }

    #[test]
    fn test_render_text_widget() {
        let w = Widget {
            id: "note".to_string(),
            widget_type: "text".to_string(),
            label: String::new(),
            value: Value::Null,
            max: 100,
            content: "Deploy OK".to_string(),
            color: "dim".to_string(),
        };
        let lines = render_widget_lines(&[w]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Deploy OK"));
    }

    #[test]
    fn test_render_empty() {
        let lines = render_widget_lines(&[]);
        assert!(lines.is_empty());
    }
}
