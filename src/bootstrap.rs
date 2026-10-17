use anyhow::{Context, Result};
use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Select};
use qrcode::{render::unicode, QrCode};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct AndroidDevice {
    pub serial: String,
    pub model: String,
    pub native_width: u16,
    pub native_height: u16,
}

#[derive(Debug, Clone)]
pub struct AdbController {
    pub adb_bin: PathBuf,
}

impl AdbController {
    /// Dynamically searches for `adb` across standard environment variables, PATH, and XDG directories.
    /// If not installed on the target PC, offers one-click portable provisioning into `$XDG_DATA_HOME/droidmonitor`.
    pub fn discover_or_provision() -> Option<Self> {
        if let Some(bin) = Self::find_existing_adb() {
            let _ = Command::new(&bin).arg("start-server").output();
            return Some(Self { adb_bin: bin });
        }

        println!(
            "\n{} O utilitário {} não foi encontrado no sistema.",
            style("ℹ").cyan().bold(),
            style("adb").yellow().bold()
        );

        let should_download = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Deseja que o DroidMonitor baixe o ADB portátil oficial do Google automaticamente (~15 MB)?")
            .default(true)
            .interact()
            .unwrap_or(false);

        if should_download {
            match Self::download_portable_adb() {
                Ok(bin) => {
                    let _ = Command::new(&bin).arg("start-server").output();
                    return Some(Self { adb_bin: bin });
                }
                Err(e) => {
                    eprintln!(
                        "{} Falha ao baixar ADB portátil: {}. Continuando em modo Rede/USB Tethering.",
                        style("⚠").yellow().bold(),
                        e
                    );
                }
            }
        }

        None
    }

    fn find_existing_adb() -> Option<PathBuf> {
        let mut candidates = Vec::new();

        if let Ok(custom) = env::var("ADB_PATH") {
            candidates.push(PathBuf::from(custom));
        }

        // Check $PATH via `which`
        if let Ok(out) = Command::new("which").arg("adb").output() {
            if out.status.success() {
                let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !p.is_empty() {
                    candidates.push(PathBuf::from(p));
                }
            }
        }

        // Check portable XDG data dir
        if let Some(data_dir) = dirs::data_dir() {
            candidates.push(data_dir.join("droidmonitor/platform-tools/adb"));
        }

        // Check standard Android SDK locations dynamically using $HOME / $ANDROID_HOME
        for env_key in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
            if let Ok(sdk) = env::var(env_key) {
                candidates.push(PathBuf::from(sdk).join("platform-tools/adb"));
            }
        }

        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join("Android/Sdk/platform-tools/adb"));
            candidates.push(home.join("android-sdk/platform-tools/adb"));
        }

        candidates.push(PathBuf::from("/usr/bin/adb"));
        candidates.push(PathBuf::from("/usr/local/bin/adb"));
        candidates.push(PathBuf::from("/opt/android-sdk/platform-tools/adb"));

        candidates.into_iter().find(|p| p.is_file())
    }

    fn download_portable_adb() -> Result<PathBuf> {
        let data_dir = dirs::data_dir()
            .context("Não foi possível obter XDG_DATA_HOME")?
            .join("droidmonitor");
        fs::create_dir_all(&data_dir)?;

        let zip_path = data_dir.join("platform-tools.zip");
        let url = "https://dl.google.com/android/repository/platform-tools-latest-linux.zip";

        println!("⬇ Baixando {}...", url);
        let status = Command::new("curl")
            .args(["-L", "-f", "-s", "-o"])
            .arg(&zip_path)
            .arg(url)
            .status()
            .context("Falha ao executar `curl` para download do platform-tools")?;

        if !status.success() {
            anyhow::bail!("Download do platform-tools retornou erro.");
        }

        println!("📦 Extraindo platform-tools em {}...", data_dir.display());
        let unzip_status = Command::new("unzip")
            .args(["-q", "-o"])
            .arg(&zip_path)
            .arg("-d")
            .arg(&data_dir)
            .status()
            .context("Falha ao executar `unzip`")?;

        let _ = fs::remove_file(&zip_path);
        if !unzip_status.success() {
            anyhow::bail!("Falha ao descompactar platform-tools.zip");
        }

        let adb_bin = data_dir.join("platform-tools/adb");
        if adb_bin.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&adb_bin, fs::Permissions::from_mode(0o755));
            }
            Ok(adb_bin)
        } else {
            anyhow::bail!("Binário adb não encontrado após extração")
        }
    }

    /// Waits for or selects an Android device connected via USB.
    pub fn wait_for_usb_device(&self) -> Option<AndroidDevice> {
        let mut printed_wait = false;
        let mut printed_unauth = false;

        loop {
            let (authorized, unauthorized) = self.scan_devices();

            if !authorized.is_empty() {
                if authorized.len() == 1 {
                    return Some(authorized.into_iter().next().unwrap());
                } else {
                    let labels: Vec<String> = authorized
                        .iter()
                        .map(|d| format!("{} ({}) - {}x{}", d.model, d.serial, d.native_width, d.native_height))
                        .collect();
                    let idx = Select::with_theme(&ColorfulTheme::default())
                        .with_prompt("Múltiplos dispositivos Android detectados. Qual deseja usar como monitor?")
                        .items(&labels)
                        .default(0)
                        .interact()
                        .unwrap_or(0);
                    return authorized.into_iter().nth(idx);
                }
            }

            if !unauthorized.is_empty() {
                if !printed_unauth {
                    println!(
                        "\n{} Dispositivo USB detectado ({}), mas aguardando autorização!",
                        style("📱").yellow(),
                        unauthorized.join(", ")
                    );
                    println!(
                        "👉 Olhe para a tela do seu celular/tablet e toque em {} na janela de Depuração USB.",
                        style("\"Permitir\"").green().bold()
                    );
                    printed_unauth = true;
                }
                thread::sleep(Duration::from_millis(1000));
                continue;
            }

            if !printed_wait {
                println!(
                    "\n{} Nenhum dispositivo Android via ADB detectado ainda.",
                    style("🔌").cyan()
                );
                println!("   Para conexão automática via cabo USB:");
                println!("   1. Conecte o celular/tablet ao PC pelo cabo USB.");
                println!("   2. No Android, ative as {} -> {}.", style("Opções do Desenvolvedor").bold(), style("Depuração USB").green().bold());
                println!("   (Ou ative a 'Ancoragem USB / USB Tethering' caso prefira conectar sem ADB)\n");

                let choices = &[
                    "🔄 Aguardar conexão do cabo USB / Depuração USB automaticamente",
                    "🌐 Continuar sem ADB (usar Ancoragem USB / Wi-Fi / QR Code)",
                ];
                let sel = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Como deseja prosseguir?")
                    .items(choices)
                    .default(0)
                    .interact()
                    .unwrap_or(0);

                if sel == 1 {
                    return None;
                }
                println!("⏳ Aguardando você plugar o dispositivo Android no USB... (Pressione Ctrl+C para cancelar)");
                printed_wait = true;
            }

            thread::sleep(Duration::from_millis(1200));
        }
    }

    fn scan_devices(&self) -> (Vec<AndroidDevice>, Vec<String>) {
        let mut authorized = Vec::new();
        let mut unauthorized = Vec::new();

        let Ok(out) = Command::new(&self.adb_bin).arg("devices").output() else {
            return (authorized, unauthorized);
        };
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines().skip(1) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                let serial = parts[0].to_string();
                let state = parts[1];
                if state == "device" {
                    let model = self
                        .query_prop(&serial, "ro.product.model")
                        .unwrap_or_else(|| "Android".to_string());
                    let (w, h) = self.query_resolution(&serial).unwrap_or((1920, 1080));
                    authorized.push(AndroidDevice {
                        serial,
                        model,
                        native_width: w,
                        native_height: h,
                    });
                } else if state == "unauthorized" {
                    unauthorized.push(serial);
                }
            }
        }

        (authorized, unauthorized)
    }

    fn query_prop(&self, serial: &str, prop: &str) -> Option<String> {
        let out = Command::new(&self.adb_bin)
            .args(["-s", serial, "shell", "getprop", prop])
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    fn query_resolution(&self, serial: &str) -> Option<(u16, u16)> {
        let out = Command::new(&self.adb_bin)
            .args(["-s", serial, "shell", "wm", "size"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        // Prefer "Override size:" if set, else "Physical size:"
        for line in text.lines().rev() {
            if let Some((_, res)) = line.split_once(':') {
                let res = res.trim();
                if let Some((w_str, h_str)) = res.split_once('x') {
                    if let (Ok(w), Ok(h)) = (w_str.parse::<u16>(), h_str.parse::<u16>()) {
                        return Some((w, h));
                    }
                }
            }
        }
        None
    }

    /// Checks if the given Android device is currently connected and in the `device` state via ADB.
    pub fn is_device_connected(&self, serial: &str) -> bool {
        let Ok(out) = Command::new(&self.adb_bin)
            .args(["-s", serial, "get-state"])
            .output() else {
                return false;
            };
        let s = String::from_utf8_lossy(&out.stdout);
        s.trim() == "device"
    }

    /// Checks if the ADB reverse tunnel for the specified port is active on the device.
    pub fn is_reverse_tunnel_active(&self, serial: &str, port: u16) -> bool {
        let Ok(out) = Command::new(&self.adb_bin)
            .args(["-s", serial, "reverse", "--list"])
            .output() else {
                return false;
            };
        let text = String::from_utf8_lossy(&out.stdout);
        let target = format!("tcp:{}", port);
        text.contains(&target)
    }

    pub fn setup_reverse_tunnel(&self, serial: &str, port: u16) -> Result<()> {
        let tcp_spec = format!("tcp:{}", port);
        let status = Command::new(&self.adb_bin)
            .args(["-s", serial, "reverse", &tcp_spec, &tcp_spec])
            .status()
            .context("Falha ao configurar `adb reverse`")?;

        if !status.success() {
            anyhow::bail!("Comando `adb reverse` falhou para o dispositivo {}", serial);
        }

        // Wake up screen on the Android device in case it is locked/off
        let _ = Command::new(&self.adb_bin)
            .args(["-s", serial, "shell", "input", "keyevent", "KEYCODE_WAKEUP"])
            .output();

        Ok(())
    }

    pub fn setup_usb_tunnel_and_open_browser(&self, serial: &str, port: u16) -> Result<()> {
        self.setup_reverse_tunnel(serial, port)?;

        // Open default browser pointing to the local USB tunnel
        let url = format!("http://127.0.0.1:{}", port);
        let _ = Command::new(&self.adb_bin)
            .args([
                "-s",
                serial,
                "shell",
                "am",
                "start",
                "-a",
                "android.intent.action.VIEW",
                "-d",
                &url,
            ])
            .output();

        Ok(())
    }

    pub fn remove_usb_tunnel(adb_bin: &Path, serial: &str, port: u16) {
        let tcp_spec = format!("tcp:{}", port);
        let _ = Command::new(adb_bin)
            .args(["-s", serial, "reverse", "--remove", &tcp_spec])
            .output();
    }
}

/// Lists non-loopback IPv4 addresses (prioritizing USB tethering interfaces like `usb0`, `rndis0`, `enp*u*`).
pub fn discover_local_ipv4s() -> Vec<(String, String)> {
    let mut results = Vec::new();
    let Ok(out) = Command::new("ip").args(["-4", "-o", "addr", "show"]).output() else {
        return results;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 {
            let iface = parts[1].to_string();
            if iface == "lo" || iface.starts_with("docker") || iface.starts_with("br-") || iface.starts_with("veth") {
                continue;
            }
            if let Some(ip_cidr) = parts.iter().skip(2).find(|&&p| p.contains('.')) {
                let ip = ip_cidr.split('/').next().unwrap_or(ip_cidr).to_string();
                results.push((iface, ip));
            }
        }
    }
    // Sort USB interfaces first
    results.sort_by_key(|(iface, _)| {
        if iface.starts_with("usb") || iface.starts_with("rndis") || iface.contains('u') {
            0
        } else {
            1
        }
    });
    results
}

pub fn print_qr_code(url: &str) {
    if let Ok(code) = QrCode::new(url.as_bytes()) {
        let image = code
            .render::<unicode::Dense1x2>()
            .dark_color(unicode::Dense1x2::Light)
            .light_color(unicode::Dense1x2::Dark)
            .build();
        println!("\n{}", image);
    }
}
