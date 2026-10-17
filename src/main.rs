mod bootstrap;
mod capture;
mod cosmic_config;
mod display;
mod edid;
mod input;
mod server;

use anyhow::{bail, Context, Result};
use bootstrap::{discover_local_ipv4s, print_qr_code, AdbController};
use clap::Parser;
use console::style;
use cosmic_config::CosmicInputMapGuard;
use dialoguer::{theme::ColorfulTheme, Select};
use display::{
    discover_free_drm_connectors, list_cosmic_outputs, run_privileged_daemon, MonitorPosition,
    VirtualDisplaySession,
};
use input::VirtualTouchscreen;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Parser, Debug)]
#[command(
    name = "droidmonitor",
    version,
    about = "Segundo monitor virtual Plug-and-Play com Multi-Touch para COSMIC Wayland + Android USB"
)]
struct Cli {
    /// Porta TCP do servidor HTTP/WebSocket
    #[arg(short, long, default_value_t = 8765)]
    port: u16,

    /// Taxa de quadros alvo (FPS)
    #[arg(short, long, default_value_t = 60)]
    fps: u32,

    /// Qualidade JPEG (30..95)
    #[arg(short, long, default_value_t = 75)]
    quality: u8,

    /// Exibe telemetria em tempo real no terminal (Wayland FPS, Dirty-Rects e Decode Android)
    #[arg(short, long, default_value_t = false)]
    debug: bool,

    /// Uso interno: daemon privilegiado invocado automaticamente via sudo
    #[arg(long, hide = true, num_args = 6)]
    internal_privileged_daemon: Option<Vec<String>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Se for invocado internamente como root helper (`sudo droidmonitor --internal-privileged-daemon ...`)
    if let Some(args) = cli.internal_privileged_daemon {
        let card_num = &args[0];
        let connector_name = &args[1];
        let width: u16 = args[2].parse().unwrap_or(1920);
        let height: u16 = args[3].parse().unwrap_or(1080);
        let refresh_hz: u16 = args[4].parse().unwrap_or(60);
        let uid: u32 = args[5].parse().unwrap_or(1000);
        return run_privileged_daemon(card_num, connector_name, width, height, refresh_hz, uid);
    }

    println!(
        "\n{}",
        style("╔══════════════════════════════════════════════════════════════╗")
            .cyan()
            .bold()
    );
    println!(
        "{}",
        style("║        🚀 DroidMonitor Wayland — COSMIC Plug & Play         ║")
            .cyan()
            .bold()
    );
    println!(
        "{}\n",
        style("╚══════════════════════════════════════════════════════════════╝")
            .cyan()
            .bold()
    );

    // 1. Auto-diagnóstico dos monitores ativos e conectores livres da GPU
    let active_outputs = list_cosmic_outputs()?
        .into_iter()
        .filter(|o| o.enabled && !o.is_droidmonitor)
        .collect::<Vec<_>>();

    if active_outputs.is_empty() {
        bail!("Nenhum monitor ativo detectado pelo `cosmic-randr list`.");
    }

    let free_connectors = discover_free_drm_connectors()?;
    if free_connectors.is_empty() {
        bail!("Nenhum conector de vídeo desconectado disponível em /sys/class/drm.");
    }

    let chosen_connector = &free_connectors[0];
    println!(
        "{} Monitor principal detectado: {} ({}x{} @ {:.0}Hz)",
        style("✔").green().bold(),
        style(&active_outputs[0].name).cyan().bold(),
        active_outputs[0].width,
        active_outputs[0].height,
        active_outputs[0].refresh_hz
    );
    println!(
        "{} Conector livre para monitor virtual: {} (card{})",
        style("✔").green().bold(),
        style(&chosen_connector.connector_name).green().bold(),
        chosen_connector.card_num
    );

    // 2. Descoberta do ADB e detecção do celular/tablet Android via USB
    let adb = AdbController::discover_or_provision();
    let android_dev = adb.as_ref().and_then(|a| a.wait_for_usb_device());

    if let Some(ref dev) = android_dev {
        println!(
            "{} Dispositivo Android conectado via USB: {} [{}] ({}x{})",
            style("✔").green().bold(),
            style(&dev.model).magenta().bold(),
            dev.serial,
            dev.native_width,
            dev.native_height
        );
    }

    // 3. Perguntas rápidas Plug-and-Play (com padrões inteligentes para só dar Enter)
    let ref_output = if active_outputs.len() == 1 {
        &active_outputs[0]
    } else {
        let items: Vec<String> = active_outputs
            .iter()
            .map(|o| format!("{} ({}x{})", o.name, o.width, o.height))
            .collect();
        let idx = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Em relação a qual monitor deseja posicionar a segunda tela?")
            .items(&items)
            .default(0)
            .interact()?;
        &active_outputs[idx]
    };

    let positions = [
        MonitorPosition::Right,
        MonitorPosition::Left,
        MonitorPosition::Below,
        MonitorPosition::Above,
    ];
    let pos_labels: Vec<&str> = positions.iter().map(|p| p.label()).collect();
    let pos_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Posição do segundo monitor virtual")
        .items(&pos_labels)
        .default(0)
        .interact()?;
    let chosen_position = positions[pos_idx];

    let orientations = ["Paisagem / Horizontal (Landscape)", "Retrato / Vertical (Portrait)"];
    let orient_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Orientação do celular/tablet")
        .items(&orientations)
        .default(0)
        .interact()?;
    let is_landscape = orient_idx == 0;

    let mut res_presets: Vec<(String, u16, u16)> = Vec::new();
    if let Some(ref dev) = android_dev {
        let (max_dim, min_dim) = (
            dev.native_width.max(dev.native_height),
            dev.native_width.min(dev.native_height),
        );
        let (nw, nh) = if is_landscape {
            (max_dim, min_dim)
        } else {
            (min_dim, max_dim)
        };
        res_presets.push((
            format!("Resolução Nativa do {} ({}x{})", dev.model, nw, nh),
            nw,
            nh,
        ));
        // Escala 75% da nativa (ótima para legibilidade em telas pequenas)
        let sw = ((nw as u32 * 3 / 4) & !7) as u16;
        let sh = ((nh as u32 * 3 / 4) & !7) as u16;
        res_presets.push((
            format!("Escala Confortável 75% do {} ({}x{})", dev.model, sw, sh),
            sw,
            sh,
        ));
    }

    if is_landscape {
        res_presets.push(("Full HD (1920x1080)".to_string(), 1920, 1080));
        res_presets.push(("HD+ (1600x900)".to_string(), 1600, 900));
        res_presets.push(("HD (1280x720 - Máxima Fluidez)".to_string(), 1280, 720));
    } else {
        res_presets.push(("Full HD Vertical (1080x1920)".to_string(), 1080, 1920));
        res_presets.push(("HD+ Vertical (900x1600)".to_string(), 900, 1600));
        res_presets.push(("HD Vertical (720x1280 - Máxima Fluidez)".to_string(), 720, 1280));
    }

    let res_labels: Vec<&str> = res_presets.iter().map(|(l, _, _)| l.as_str()).collect();
    let res_idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Escolha a resolução do monitor virtual")
        .items(&res_labels)
        .default(0)
        .interact()?;
    let (_, target_w, target_h) = res_presets[res_idx];

    // 4. Ativar o monitor virtual com clock de 120Hz (VBlank de 8.33ms)!
    // Como a máquina de estados KMS do cosmic-comp leva 2 VBlanks por captura com PaintCursors,
    // 120Hz no EDID (sem Range Limits e limpando outputs.ron antes) reduz o wait do Wayland
    // de 33.1ms (30 FPS) para 16.6ms (60 FPS cravados)!
    let edid_refresh_hz = (cli.fps * 2).clamp(60, 120) as u16;
    let mut display_session = VirtualDisplaySession::activate(
        chosen_connector,
        ref_output,
        chosen_position,
        target_w,
        target_h,
        edid_refresh_hz,
    )?;

    println!(
        "{} Segundo monitor {} ativado em {}x{} @ {:.0}Hz na posição ({}, {})!",
        style("✔").green().bold(),
        style(&display_session.connector_name).green().bold(),
        display_session.width,
        display_session.height,
        display_session.refresh_hz,
        display_session.x,
        display_session.y
    );

    // 5. Criar Touchscreen Virtual Multi-Touch (/dev/uinput) e vinculá-lo ao 2º monitor no COSMIC
    let touch_dev = Arc::new(Mutex::new(
        VirtualTouchscreen::new().context("Falha ao inicializar Touchscreen Virtual")?,
    ));
    let mut cosmic_map_guard =
        CosmicInputMapGuard::bind_touch_to_output(&display_session.connector_name)?;
    println!(
        "{} Touchscreen Multi-Touch virtual criado e vinculado exclusivamente ao monitor {}!",
        style("✔").green().bold(),
        style(&display_session.connector_name).green().bold()
    );

    // 6. Iniciar thread de captura Wayland exclusiva para o monitor virtual (com Dirty-Rectangle Delta)
    let (packet_tx, _) = tokio::sync::broadcast::channel(32);
    let latest_keyframe = Arc::new(std::sync::RwLock::new(None));
    let diagnostics = Arc::new(capture::ClientDiagnostics::default());
    diagnostics.debug_log.store(
        cli.debug || std::env::var("DROIDMONITOR_DEBUG").is_ok(),
        Ordering::Relaxed,
    );
    let capture_packet_tx = packet_tx.clone();
    let capture_keyframe = latest_keyframe.clone();
    let capture_diag = diagnostics.clone();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let capture_stop = stop_flag.clone();
    let capture_output_name = display_session.connector_name.clone();
    let fps = cli.fps;
    let quality = cli.quality.clamp(30, 95);

    let capture_handle = std::thread::spawn(move || {
        if let Err(e) = capture::run_wayland_capture_loop(
            capture_output_name,
            fps,
            quality,
            capture_packet_tx,
            capture_keyframe,
            capture_diag,
            capture_stop,
        ) {
            eprintln!("⚠ Erro no loop de captura Wayland: {:#}", e);
        }
    });

    // 7. Iniciar servidor HTTP + WebSocket e abrir túnel USB no Android
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_port = cli.port;
    let server_task = tokio::spawn(async move {
        let _ = server::run_http_ws_server(
            server_port,
            is_landscape,
            packet_tx,
            latest_keyframe,
            diagnostics,
            touch_dev,
            shutdown_rx,
        )
        .await;
    });

    if let (Some(adb_ctrl), Some(dev)) = (&adb, &android_dev) {
        match adb_ctrl.setup_usb_tunnel_and_open_browser(&dev.serial, cli.port) {
            Ok(_) => {
                println!(
                    "\n{} Túnel USB (adb reverse) ativo e navegador aberto automaticamente no {}!",
                    style("🚀").green().bold(),
                    style(&dev.model).magenta().bold()
                );
                println!(
                    "   Caso o navegador não tenha aberto sozinho, acesse no Android: {}",
                    style(format!("http://127.0.0.1:{}", cli.port)).cyan().underlined()
                );
            }
            Err(e) => {
                eprintln!("⚠ Aviso ao abrir navegador via ADB: {}", e);
            }
        }
    }

    let local_ips = discover_local_ipv4s();
    if !local_ips.is_empty() {
        println!("\n🌐 Endereços alternativos (USB Tethering / Wi-Fi):");
        for (iface, ip) in &local_ips {
            println!(
                "   • [{}] {}",
                style(iface).yellow(),
                style(format!("http://{}:{}", ip, cli.port)).cyan().underlined()
            );
        }
        if android_dev.is_none() {
            let first_url = format!("http://{}:{}", local_ips[0].1, cli.port);
            print_qr_code(&first_url);
        }
    }

    println!(
        "\n{} Pressione {} a qualquer momento para encerrar e desconectar o monitor virtual com segurança.\n",
        style("ℹ").blue().bold(),
        style("Ctrl+C").yellow().bold()
    );

    // 8. Aguardar Ctrl+C e executar rollback completo e limpo
    let _ = tokio::signal::ctrl_c().await;
    println!("\n🛑 Encerrando DroidMonitor e restaurando configuração original de telas...");

    stop_flag.store(true, Ordering::Relaxed);
    let _ = shutdown_tx.send(true);

    if let (Some(adb_ctrl), Some(dev)) = (&adb, &android_dev) {
        AdbController::remove_usb_tunnel(&adb_ctrl.adb_bin, &dev.serial, cli.port);
    }

    cosmic_map_guard.restore();
    display_session.shutdown();

    let _ = capture_handle.join();
    let _ = server_task.await;

    println!("{} Monitor virtual removido e sistema restaurado com sucesso!", style("✔").green().bold());
    Ok(())
}
