use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

pub const TOUCHSCREEN_DEVICE_NAME: &str = "DroidMonitor Virtual Touchscreen";

pub struct CosmicInputMapGuard {
    config_path: PathBuf,
    original_content: Option<String>,
}

/// Immediately purges any `"DroidMonitor"` entries from `~/.local/state/cosmic-comp/outputs.ron`
/// so `cosmic-comp` never tries to apply stale modes/refresh-rates from previous sessions.
pub fn purge_droidmonitor_from_outputs_ron() {
    let outputs_state_path = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .map(|d| d.join("cosmic-comp/outputs.ron"));

    if let Some(p) = outputs_state_path {
        if p.exists() {
            if let Ok(content) = fs::read_to_string(&p) {
                let cleaned = clean_droidmonitor_from_outputs_ron(&content);
                if cleaned != content {
                    let _ = fs::write(&p, cleaned);
                }
            }
        }
    }
}

impl CosmicInputMapGuard {
    /// Cleans any leftover `"DroidMonitor"` entries from `~/.local/state/cosmic-comp/outputs.ron`
    /// and maps `"DroidMonitor Virtual Touchscreen"` to `output_name` inside
    /// `$XDG_CONFIG_HOME/cosmic/com.system76.CosmicComp/v1/input_devices`.
    pub fn bind_touch_to_output(output_name: &str) -> Result<Self> {
        purge_droidmonitor_from_outputs_ron();

        // Backup and update input_devices
        let base_cfg =
            dirs::config_dir().context("Não foi possível localizar o diretório XDG_CONFIG_HOME")?;
        let dir = base_cfg.join("cosmic/com.system76.CosmicComp/v1");
        fs::create_dir_all(&dir).ok();
        let config_path = dir.join("input_devices");

        let original_content = if config_path.exists() {
            Some(
                fs::read_to_string(&config_path)
                    .map(|c| remove_existing_device_entry(&c, TOUCHSCREEN_DEVICE_NAME))
                    .unwrap_or_default(),
            )
        } else {
            None
        };

        let entry_block = format!(
            r#"    "{device}": (
        state: Enabled,
        acceleration: None,
        calibration: None,
        click_method: None,
        disable_while_typing: None,
        left_handed: None,
        middle_button_emulation: None,
        rotation_angle: None,
        scroll_config: None,
        tap_config: None,
        map_to_output: Some("{output}"),
    ),"#,
            device = TOUCHSCREEN_DEVICE_NAME,
            output = output_name
        );

        let new_content = match &original_content {
            Some(existing) if !existing.trim().is_empty() => {
                let trimmed = existing.trim();
                if let Some(stripped) = trimmed.strip_suffix('}') {
                    let prefix = stripped.trim_end();
                    if prefix == "{" {
                        format!("{{\n{}\n}}\n", entry_block)
                    } else if prefix.ends_with(',') {
                        format!("{}\n{}\n}}\n", prefix, entry_block)
                    } else {
                        format!("{},\n{}\n}}\n", prefix, entry_block)
                    }
                } else {
                    format!("{{\n{}\n}}\n", entry_block)
                }
            }
            _ => format!("{{\n{}\n}}\n", entry_block),
        };

        fs::write(&config_path, new_content)
            .with_context(|| format!("Falha ao gravar {}", config_path.display()))?;

        Ok(Self {
            config_path,
            original_content,
        })
    }

    pub fn restore(&mut self) {
        if let Some(orig) = self.original_content.take() {
            if orig.trim().is_empty() {
                let _ = fs::write(&self.config_path, "{\n}\n");
            } else {
                let _ = fs::write(&self.config_path, orig);
            }
        } else if self.config_path.exists() {
            let _ = fs::write(&self.config_path, "{\n}\n");
        }

        purge_droidmonitor_from_outputs_ron();
    }
}

impl Drop for CosmicInputMapGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Removes any key-value block in `~/.local/state/cosmic-comp/outputs.ron` that references `"DroidMonitor"`.
pub fn clean_droidmonitor_from_outputs_ron(ron_text: &str) -> String {
    if !ron_text.contains("DroidMonitor") {
        return ron_text.to_string();
    }
    let lines: Vec<&str> = ron_text.lines().collect();
    let mut result = Vec::with_capacity(lines.len());
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Start of a topology key block at indentation level 8: `[`
        if trimmed == "[" && lines[i].starts_with("        [") {
            let start = i;
            let mut end = i;
            let mut bracket_depth = 0i32;
            let mut saw_value_array = false;

            while end < lines.len() {
                for ch in lines[end].chars() {
                    if ch == '[' {
                        bracket_depth += 1;
                    } else if ch == ']' {
                        bracket_depth -= 1;
                    }
                }
                if lines[end].contains("]: [") {
                    saw_value_array = true;
                }
                if saw_value_array && bracket_depth == 0 {
                    break;
                }
                end += 1;
            }

            if end < lines.len() {
                let block = lines[start..=end].join("\n");
                if block.contains("DroidMonitor") {
                    i = end + 1;
                    continue;
                }
            }
        }
        result.push(lines[i]);
        i += 1;
    }

    let mut out = result.join("\n");
    out.push('\n');
    out
}

fn remove_existing_device_entry(ron_text: &str, device_name: &str) -> String {
    let needle = format!("\"{}\":", device_name);
    let Some(start_idx) = ron_text.find(&needle) else {
        return ron_text.to_string();
    };
    let line_start = ron_text[..start_idx]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let rest = &ron_text[start_idx..];
    let mut depth = 0i32;
    let mut end_offset = rest.len();
    for (idx, ch) in rest.char_indices() {
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
            if depth == 0 {
                end_offset = idx + 1;
                break;
            }
        }
    }
    let after = &rest[end_offset..];
    let skip_extra = after
        .find('\n')
        .map(|i| i + 1)
        .unwrap_or(after.len());
    format!("{}{}", &ron_text[..line_start], &after[skip_extra..])
}
