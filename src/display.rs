use crate::edid::generate_edid;
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorPosition {
    Right,
    Left,
    Above,
    Below,
}

impl MonitorPosition {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Right => "Direita do monitor principal",
            Self::Left => "Esquerda do monitor principal",
            Self::Above => "Acima do monitor principal",
            Self::Below => "Abaixo do monitor principal",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActiveOutputInfo {
    pub name: String,
    pub enabled: bool,
    pub is_droidmonitor: bool,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f32,
}

#[derive(Debug, Clone)]
pub struct DrmConnectorCandidate {
    pub card_num: String,
    pub connector_name: String,
}

/// Strips ANSI escape sequences from command output (`cosmic-randr list` outputs color codes).
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parses `cosmic-randr list` dynamically to discover all currently connected monitors and their geometry.
pub fn list_cosmic_outputs() -> Result<Vec<ActiveOutputInfo>> {
    let output = Command::new("cosmic-randr")
        .arg("list")
        .output()
        .context("Falha ao executar `cosmic-randr list`. Verifique se está rodando no COSMIC Desktop.")?;

    let text = strip_ansi(&String::from_utf8_lossy(&output.stdout));
    let mut outputs = Vec::new();
    let mut current: Option<ActiveOutputInfo> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            if let Some(prev) = current.take() {
                outputs.push(prev);
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let Some(&name) = parts.first() {
                let enabled = !line.contains("(disabled)");
                current = Some(ActiveOutputInfo {
                    name: name.to_string(),
                    enabled,
                    is_droidmonitor: false,
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60.0,
                });
            }
        } else if let Some(ref mut cur) = current {
            let trimmed = line.trim();
            if let Some(model_str) = trimmed.strip_prefix("Model:") {
                if model_str.contains("DroidMonitor") {
                    cur.is_droidmonitor = true;
                }
            } else if let Some(pos_str) = trimmed.strip_prefix("Position:") {
                let coords: Vec<&str> = pos_str.trim().split(',').collect();
                if coords.len() == 2 {
                    cur.x = coords[0].trim().parse().unwrap_or(0);
                    cur.y = coords[1].trim().parse().unwrap_or(0);
                }
            } else if trimmed.contains("(current)") || (cur.width == 1920 && trimmed.contains("(preferred)")) {
                let tokens: Vec<&str> = trimmed.split_whitespace().collect();
                if let Some(res) = tokens.first() {
                    if let Some((w_str, h_str)) = res.split_once('x') {
                        if let (Ok(w), Ok(h)) = (w_str.parse::<u32>(), h_str.parse::<u32>()) {
                            cur.width = w;
                            cur.height = h;
                        }
                    }
                }
                if let Some(at_idx) = tokens.iter().position(|&t| t == "@") {
                    if let Some(hz_str) = tokens.get(at_idx + 1) {
                        if let Ok(hz) = hz_str.parse::<f32>() {
                            cur.refresh_hz = hz;
                        }
                    }
                }
            }
        }
    }

    if let Some(last) = current.take() {
        outputs.push(last);
    }

    Ok(outputs)
}

/// Checks if a DRM connector currently has our synthetic `"DroidMonitor"` EDID injected.
fn has_droidmonitor_edid(sysfs_conn_path: &std::path::Path) -> bool {
    let edid_file = sysfs_conn_path.join("edid");
    if let Ok(bytes) = fs::read(&edid_file) {
        if bytes.windows(12).any(|w| w == b"DroidMonitor") {
            return true;
        }
    }
    false
}

/// Scans `/sys/class/drm/card*-*` dynamically to find disconnected physical connectors
/// (or connectors with a leftover DroidMonitor EDID from an interrupted session).
/// Falls back to a `vkms` candidate if all physical connectors are occupied by real monitors.
pub fn discover_free_drm_connectors() -> Result<Vec<DrmConnectorCandidate>> {
    let mut candidates = Vec::new();
    let drm_dir = PathBuf::from("/sys/class/drm");
    if !drm_dir.exists() {
        return Ok(candidates);
    }

    let mut entries: Vec<_> = fs::read_dir(&drm_dir)?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let fname = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = fname.strip_prefix("card") else {
            continue;
        };
        let Some((card_num, connector_name)) = rest.split_once('-') else {
            continue;
        };
        if !card_num.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if connector_name.starts_with("Writeback") {
            continue;
        }

        let status_path = entry.path().join("status");
        let is_disconnected = fs::read_to_string(&status_path)
            .map(|s| s.trim() == "disconnected")
            .unwrap_or(false);

        if is_disconnected || has_droidmonitor_edid(&entry.path()) {
            candidates.push(DrmConnectorCandidate {
                card_num: card_num.to_string(),
                connector_name: connector_name.to_string(),
            });
        }
    }

    if candidates.is_empty() {
        candidates.push(DrmConnectorCandidate {
            card_num: "vkms".to_string(),
            connector_name: "Virtual-1".to_string(),
        });
    }

    Ok(candidates)
}

/// Privileged helper daemon communication handle.
pub struct VirtualDisplaySession {
    pub connector_name: String,
    pub width: u16,
    pub height: u16,
    pub refresh_hz: f32,
    pub x: i32,
    pub y: i32,
    helper_child: Option<Child>,
}

impl VirtualDisplaySession {
    pub fn activate(
        candidate: &DrmConnectorCandidate,
        reference_output: &ActiveOutputInfo,
        position: MonitorPosition,
        width: u16,
        height: u16,
        refresh_hz: u16,
    ) -> Result<Self> {
        // Clean any stale 60Hz DroidMonitor mode cached in `~/.local/state/cosmic-comp/outputs.ron`
        // BEFORE triggering the DRM hotplug so cosmic-comp adopts our 120Hz EDID mode cleanly!
        crate::cosmic_config::purge_droidmonitor_from_outputs_ron();

        let exe = std::env::current_exe().context("Não foi possível determinar o caminho do executável atual")?;
        let uid = nix::unistd::getuid().as_raw();

        println!(
            "🔒 Solicitando permissão de administrador (sudo) para ativar o conector virtual {} e liberar o Touchscreen (/dev/uinput)...",
            candidate.connector_name
        );

        // Validate/refresh sudo credentials in the foreground process group first!
        let auth_status = Command::new("sudo")
            .arg("-v")
            .status()
            .context("Falha ao solicitar autenticação sudo")?;
        if !auth_status.success() {
            bail!("Autenticação sudo não autorizada.");
        }

        // Spawn sudo in its own process group (`process_group(0)`) so terminal SIGINT (Ctrl+C)
        // is handled exclusively by the parent process and doesn't kill sudo before cleanup!
        let mut child = Command::new("sudo")
            .process_group(0)
            .arg("-n")
            .arg(&exe)
            .arg("--internal-privileged-daemon")
            .arg(&candidate.card_num)
            .arg(&candidate.connector_name)
            .arg(width.to_string())
            .arg(height.to_string())
            .arg(refresh_hz.to_string())
            .arg(uid.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .context("Falha ao executar sudo para o helper privilegiado")?;

        let connector_name = candidate.connector_name.clone();
        let mut detected_enabled = false;
        let mut detected = false;
        let mut reported_hz = refresh_hz as f32;
        for _ in 0..50 {
            thread::sleep(Duration::from_millis(150));
            if let Ok(outputs) = list_cosmic_outputs() {
                if let Some(o) = outputs.iter().find(|o| o.name == connector_name) {
                    detected = true;
                    detected_enabled = o.enabled;
                    reported_hz = o.refresh_hz;
                    break;
                }
            }
        }

        if !detected {
            let _ = child.kill();
            bail!(
                "O compositor COSMIC não detectou o conector virtual {} após forçar o hotplug DRM.",
                connector_name
            );
        }

        let (target_x, target_y) = match position {
            MonitorPosition::Right => (reference_output.x + reference_output.width as i32, reference_output.y),
            MonitorPosition::Left => (reference_output.x - width as i32, reference_output.y),
            MonitorPosition::Above => (reference_output.x, reference_output.y - height as i32),
            MonitorPosition::Below => (reference_output.x, reference_output.y + reference_output.height as i32),
        };

        if !detected_enabled {
            let _ = Command::new("cosmic-randr")
                .args(["enable", &connector_name])
                .output();
            thread::sleep(Duration::from_millis(150));
        }

        // Configure mode, refresh rate, disable adaptive-sync/VRR, and position in a single atomic call
        let mode_res = Command::new("cosmic-randr")
            .args([
                "mode",
                "--adaptive-sync=false",
                &format!("--refresh={:.3}", reported_hz),
                &format!("--pos-x={}", target_x),
                &format!("--pos-y={}", target_y),
                &connector_name,
                &width.to_string(),
                &height.to_string(),
            ])
            .output();

        if mode_res.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
            let _ = Command::new("cosmic-randr")
                .args([
                    "position",
                    &connector_name,
                    &target_x.to_string(),
                    &target_y.to_string(),
                ])
                .output();
        }

        thread::sleep(Duration::from_millis(250));

        let (mut final_w, mut final_h, mut final_hz) = (width, height, reported_hz);
        if let Ok(outputs) = list_cosmic_outputs() {
            if let Some(vo) = outputs.iter().find(|o| o.name == connector_name) {
                final_w = vo.width as u16;
                final_h = vo.height as u16;
                final_hz = vo.refresh_hz;
            }
        }

        Ok(Self {
            connector_name,
            width: final_w,
            height: final_h,
            refresh_hz: final_hz,
            x: target_x,
            y: target_y,
            helper_child: Some(child),
        })
    }

    pub fn shutdown(&mut self) {
        if let Some(mut child) = self.helper_child.take() {
            // 1. Disable in cosmic-randr first while still present
            let _ = Command::new("cosmic-randr")
                .args(["disable", &self.connector_name])
                .output();

            // 2. Tell the root helper daemon to disconnect the DRM connector and reset EDID
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(b"stop\n");
                let _ = stdin.flush();
            }
            let _ = child.wait();
            crate::cosmic_config::purge_droidmonitor_from_outputs_ron();
        }
    }
}

impl Drop for VirtualDisplaySession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Dynamically locates `/sys/kernel/debug/dri/<id>/<connector_name>`, handling cases where
/// the debugfs directory is indexed as `0`, `1`, or by PCI bus address.
fn resolve_debugfs_connector_dir(card_num: &str, connector_name: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(format!("/sys/kernel/debug/dri/{}/{}", card_num, connector_name));
    if direct.exists() {
        return Some(direct);
    }
    let dri_root = PathBuf::from("/sys/kernel/debug/dri");
    if let Ok(entries) = fs::read_dir(&dri_root) {
        for entry in entries.filter_map(|e| e.ok()) {
            let candidate = entry.path().join(connector_name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Disconnects a forced DRM connector and clears its EDID override so it vanishes completely from the system.
fn reset_drm_connector(card_num: &str, connector_name: &str) {
    let sysfs_base = PathBuf::from(format!("/sys/class/drm/card{}-{}", card_num, connector_name));

    if let Some(debug_base) = resolve_debugfs_connector_dir(card_num, connector_name) {
        let force_path = debug_base.join("force");
        if force_path.exists() {
            let _ = fs::write(&force_path, b"unspecified\n");
        }

        let edid_path = debug_base.join("edid_override");
        if edid_path.exists() {
            let _ = fs::write(&edid_path, b"reset\n");
        }
    }

    let status_path = sysfs_base.join("status");
    if status_path.exists() {
        // Writing "detect" to /sys/class/drm/cardX-Y/status triggers the kernel's
        // `status_store` -> `drm_helper_probe_single_connector_modes`, which immediately
        // transitions the connector back to `disconnected` and emits a DRM hotplug uevent!
        let _ = fs::write(&status_path, b"detect\n");
    }
}

/// Sweeps all connectors on `card_num` and resets any leftover `DroidMonitor` virtual screens.
fn cleanup_stale_droidmonitor_connectors(card_num: &str) {
    let drm_dir = PathBuf::from("/sys/class/drm");
    let Ok(entries) = fs::read_dir(&drm_dir) else {
        return;
    };
    let prefix = format!("card{}-", card_num);
    for entry in entries.filter_map(|e| e.ok()) {
        let fname = entry.file_name().to_string_lossy().to_string();
        if let Some(conn_name) = fname.strip_prefix(&prefix) {
            if has_droidmonitor_edid(&entry.path()) {
                reset_drm_connector(card_num, conn_name);
            }
        }
    }
}

/// Runs inside `sudo droidmonitor --internal-privileged-daemon ...`
pub fn run_privileged_daemon(
    card_num: &str,
    connector_name: &str,
    width: u16,
    height: u16,
    refresh_hz: u16,
    uid: u32,
) -> Result<()> {
    // Ignore SIGINT so Ctrl+C in the user's terminal doesn't kill this daemon before it rolls back
    unsafe {
        nix::libc::signal(nix::libc::SIGINT, nix::libc::SIG_IGN);
    }

    let _ = Command::new("modprobe").arg("uinput").status();
    let uinput_path = PathBuf::from("/dev/uinput");
    if uinput_path.exists() {
        let status = Command::new("setfacl")
            .args(["-m", &format!("u:{}:rw", uid), "/dev/uinput"])
            .status();
        if status.is_err() || !status.unwrap().success() {
            let _ = fs::set_permissions(
                &uinput_path,
                std::os::unix::fs::PermissionsExt::from_mode(0o666),
            );
        }
    }

    if card_num == "vkms" {
        let _ = Command::new("modprobe").arg("vkms").status();
        let _ = Command::new("udevadm")
            .args(["trigger", "--action=change", "--subsystem-match=drm"])
            .status();
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);
        let _ = Command::new("modprobe").args(["-r", "vkms"]).status();
        return Ok(());
    }

    // First, clean up any leftover virtual screens from previous sessions
    cleanup_stale_droidmonitor_connectors(card_num);

    let debug_base = resolve_debugfs_connector_dir(card_num, connector_name).with_context(|| {
        format!(
            "Diretório debugfs para o conector {} (card{}) não encontrado em /sys/kernel/debug/dri.",
            connector_name, card_num
        )
    })?;

    let edid_bytes = generate_edid(width, height, refresh_hz);
    let edid_path = debug_base.join("edid_override");
    if edid_path.exists() {
        let _ = fs::write(&edid_path, &edid_bytes);
    }

    let force_path = debug_base.join("force");
    fs::write(&force_path, b"on\n")
        .with_context(|| format!("Falha ao escrever 'on' em {}", force_path.display()))?;

    let status_path = PathBuf::from(format!("/sys/class/drm/card{}-{}/status", card_num, connector_name));
    if status_path.exists() {
        let _ = fs::write(&status_path, b"on\n");
    }

    let _ = Command::new("udevadm")
        .args(["trigger", "--action=change", "--subsystem-match=drm"])
        .status();

    // Block until parent writes "stop\n" or closes the pipe (even if parent is killed!)
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);

    // Complete rollback of the connector and any other DroidMonitor connectors
    reset_drm_connector(card_num, connector_name);
    cleanup_stale_droidmonitor_connectors(card_num);

    let _ = Command::new("udevadm")
        .args(["trigger", "--action=change", "--subsystem-match=drm"])
        .status();

    Ok(())
}
