//! Otimizador Linux — analisa o PC, sugere melhorias e recomenda a melhor distro.
//! Nada é aplicado automaticamente: o app só sugere e copia comandos.

use eframe::egui::{self, Color32, RichText};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use std::{fs, thread};
use sysinfo::{Disks, System};

// ───────────────────────────── Modelos ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Sev {
    Info,
    Aviso,
    Importante,
}

impl Sev {
    fn label(self) -> &'static str {
        match self {
            Sev::Info => "DICA",
            Sev::Aviso => "ATENÇÃO",
            Sev::Importante => "IMPORTANTE",
        }
    }
    fn color(self) -> Color32 {
        match self {
            Sev::Info => Color32::from_rgb(110, 170, 255),
            Sev::Aviso => Color32::from_rgb(240, 190, 70),
            Sev::Importante => Color32::from_rgb(240, 100, 100),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cat {
    Bateria,
    Cpu,
    Disco,
    Memoria,
    Sistema,
}

impl Cat {
    const ALL: [Cat; 5] = [Cat::Bateria, Cat::Cpu, Cat::Disco, Cat::Memoria, Cat::Sistema];
    fn label(self) -> &'static str {
        match self {
            Cat::Bateria => "Bateria",
            Cat::Cpu => "CPU",
            Cat::Disco => "Disco",
            Cat::Memoria => "Memória",
            Cat::Sistema => "Sistema",
        }
    }
}

#[derive(Clone)]
struct Suggestion {
    cat: Cat,
    sev: Sev,
    title: String,
    why: String,
    cmd: Option<String>,
}

#[derive(Clone, Default)]
struct DiskInfo {
    name: String,
    ssd: bool,
    size_gb: f64,
}

#[derive(Clone, Default)]
struct Battery {
    health: Option<f32>,
    pct: Option<u32>,
    status: String,
    watts: Option<f32>,
}

#[derive(Clone, Default)]
struct Report {
    cpu_model: String,
    cores: usize,
    cpu_usage: f32,
    ram_gb: f64,
    ram_used_pct: f32,
    swap_gb: f64,
    distro: String,
    distro_id: String,
    kernel: String,
    desktop: String,
    gpu: String,
    has_nvidia: bool,
    battery: Option<Battery>,
    disks: Vec<DiskInfo>,
    root_used_pct: Option<f32>,
    boot_secs: Option<f32>,
    top_cpu: Vec<(String, f32, u64)>,
    top_mem: Vec<(String, f32, u64)>,
    suggestions: Vec<Suggestion>,
    score: u8,
}

// ───────────────────────────── Utilitários ─────────────────────────────

fn rd(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn run(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn has(cmd: &str) -> bool {
    let mut dirs: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(String::from)
        .collect();
    dirs.extend(["/usr/sbin", "/sbin", "/usr/local/sbin"].map(String::from));
    dirs.iter().any(|d| Path::new(d).join(cmd).exists())
}

fn enabled(unit: &str) -> bool {
    run("systemctl", &["is-enabled", unit]) == "enabled"
}

fn os_release() -> (String, String) {
    let txt = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let get = |k: &str| {
        let prefix = format!("{k}=");
        txt.lines()
            .find_map(|l| l.strip_prefix(prefix.as_str()).map(|v| v.trim_matches('"').to_string()))
            .unwrap_or_default()
    };
    (get("PRETTY_NAME"), get("ID"))
}

fn package_manager() -> &'static str {
    for pm in ["apt", "dnf", "pacman", "zypper"] {
        if has(pm) {
            return pm;
        }
    }
    "?"
}

fn install_cmd(pm: &str, pkgs: &str) -> String {
    match pm {
        "apt" => format!("sudo apt install -y {pkgs}"),
        "dnf" => format!("sudo dnf install -y {pkgs}"),
        "pacman" => format!("sudo pacman -S --needed {pkgs}"),
        "zypper" => format!("sudo zypper install {pkgs}"),
        _ => format!("# instale com o gerenciador de pacotes da sua distro: {pkgs}"),
    }
}

fn parse_boot_secs(out: &str) -> Option<f32> {
    let line = out.lines().find(|l| l.starts_with("Startup finished"))?;
    let tail = line.rsplit('=').next()?.trim();
    let mut total = 0.0;
    for tok in tail.split_whitespace() {
        if let Some(v) = tok.strip_suffix("min") {
            total += v.parse::<f32>().ok()? * 60.0;
        } else if let Some(v) = tok.strip_suffix("ms") {
            total += v.parse::<f32>().ok()? / 1000.0;
        } else if let Some(v) = tok.strip_suffix('s') {
            total += v.parse::<f32>().ok()?;
        }
    }
    Some(total)
}

fn wifi_iface() -> Option<String> {
    fs::read_dir("/sys/class/net")
        .ok()?
        .flatten()
        .find(|e| e.path().join("wireless").exists())
        .map(|e| e.file_name().to_string_lossy().to_string())
}

fn read_battery() -> Option<(String, Battery)> {
    for e in fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let p = e.path();
        if rd(&p.join("type")).as_deref() != Some("Battery") {
            continue;
        }
        if rd(&p.join("scope")).as_deref() == Some("Device") {
            continue; // bateria de mouse/teclado
        }
        let num = |n: &str| rd(&p.join(n)).and_then(|s| s.parse::<f64>().ok());
        let full = num("energy_full").or(num("charge_full"));
        let design = num("energy_full_design").or(num("charge_full_design"));
        let health = match (full, design) {
            (Some(f), Some(d)) if d > 0.0 => Some((f / d * 100.0) as f32),
            _ => None,
        };
        let watts = num("power_now").map(|w| (w / 1e6) as f32).or_else(|| {
            match (num("current_now"), num("voltage_now")) {
                (Some(c), Some(v)) => Some((c * v / 1e12) as f32),
                _ => None,
            }
        });
        let b = Battery {
            health,
            pct: num("capacity").map(|c| c as u32),
            status: rd(&p.join("status")).unwrap_or_default(),
            watts,
        };
        return Some((e.file_name().to_string_lossy().to_string(), b));
    }
    None
}

fn sg(cat: Cat, sev: Sev, title: &str, why: String, cmd: Option<String>) -> Suggestion {
    Suggestion { cat, sev, title: title.to_string(), why, cmd }
}

// ───────────────────────────── Análise ─────────────────────────────

fn scan() -> Report {
    let mut sys = System::new_all();
    thread::sleep(Duration::from_millis(1200)); // amostra de uso de CPU
    sys.refresh_all();

    let mut r = Report::default();
    let gb = 1024f64.powi(3);
    let pm = package_manager();

    // Hardware básico
    r.cpu_model = sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default();
    r.cores = sys.cpus().len().max(1);
    r.cpu_usage = sys.cpus().iter().map(|c| c.cpu_usage()).sum::<f32>() / r.cores as f32;
    r.ram_gb = sys.total_memory() as f64 / gb;
    r.ram_used_pct = if sys.total_memory() > 0 {
        sys.used_memory() as f32 / sys.total_memory() as f32 * 100.0
    } else {
        0.0
    };
    r.swap_gb = sys.total_swap() as f64 / gb;
    let (d, id) = os_release();
    r.distro = d;
    r.distro_id = id;
    r.kernel = System::kernel_version().unwrap_or_default();
    r.desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "desconhecido".into());

    // GPU
    let gpus: Vec<String> = run("lspci", &[])
        .lines()
        .filter(|l| l.contains("VGA") || l.contains("3D controller") || l.contains("Display controller"))
        .filter_map(|l| l.splitn(2, ": ").nth(1).map(String::from))
        .collect();
    r.has_nvidia = gpus.iter().any(|g| g.to_lowercase().contains("nvidia"));
    r.gpu = if gpus.is_empty() { "não identificada".into() } else { gpus.join(" / ") };

    // Discos
    if let Ok(rdir) = fs::read_dir("/sys/block") {
        for e in rdir.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.starts_with("sd") || name.starts_with("nvme") || name.starts_with("mmcblk") || name.starts_with("vd")) {
                continue;
            }
            let p = e.path();
            let rotational = rd(&p.join("queue/rotational")).as_deref() == Some("1");
            let sectors = rd(&p.join("size")).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            r.disks.push(DiskInfo { name, ssd: !rotational, size_gb: sectors * 512.0 / 1e9 });
        }
    }
    let disks = Disks::new_with_refreshed_list();
    if let Some(root) = disks.iter().find(|d| d.mount_point() == Path::new("/")) {
        let total = root.total_space() as f32;
        if total > 0.0 {
            r.root_used_pct = Some((1.0 - root.available_space() as f32 / total) * 100.0);
        }
    }

    // Processos (agrupados por nome; CPU em % do total da máquina)
    let mut agg: HashMap<String, (f32, u64)> = HashMap::new();
    for p in sys.processes().values() {
        let e = agg.entry(p.name().to_string_lossy().to_string()).or_insert((0.0, 0));
        e.0 += p.cpu_usage();
        e.1 += p.memory();
    }
    let all: Vec<(String, f32, u64)> =
        agg.into_iter().map(|(n, (c, m))| (n, c / r.cores as f32, m)).collect();
    let mut by_cpu = all.clone();
    by_cpu.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut by_mem = all;
    by_mem.sort_by(|a, b| b.2.cmp(&a.2));
    r.top_cpu = by_cpu.into_iter().take(8).collect();
    r.top_mem = by_mem.into_iter().take(8).collect();

    // Boot
    r.boot_secs = parse_boot_secs(&run("systemd-analyze", &[]));

    // Fatos de energia
    let governor = rd(Path::new("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"));
    let tlp = has("tlp");
    let ppd = has("powerprofilesctl") || enabled("power-profiles-daemon.service");
    let auto_cpufreq = has("auto-cpufreq");
    let is_intel = r.cpu_model.contains("Intel");
    let battery = read_battery();
    r.battery = battery.as_ref().map(|(_, b)| b.clone());

    let mut s: Vec<Suggestion> = Vec::new();

    // ── Bateria ──
    if let Some((bat_name, b)) = &battery {
        if !tlp && !ppd && !auto_cpufreq {
            s.push(sg(
                Cat::Bateria,
                Sev::Importante,
                "Ative um gerenciador de energia",
                "Seu notebook não tem TLP, power-profiles-daemon nem auto-cpufreq. Sem eles, CPU, Wi-Fi, USB e disco \
                 ficam em configurações genéricas e a bateria dura menos. O TLP já aplica bons ajustes sozinho."
                    .into(),
                Some(format!("{} && sudo systemctl enable --now tlp", install_cmd(pm, "tlp"))),
            ));
        }
        if governor.as_deref() == Some("performance") {
            let cmd = if ppd {
                "powerprofilesctl set power-saver".to_string()
            } else {
                "sudo cpupower frequency-set -g powersave".to_string()
            };
            s.push(sg(
                Cat::Bateria,
                Sev::Aviso,
                "CPU travada no modo desempenho",
                "O governador da CPU está em “performance”, que mantém a frequência alta o tempo todo e gasta bateria \
                 (e esquenta). Use economia de energia quando estiver na bateria."
                    .into(),
                Some(cmd),
            ));
        }
        if let Some(h) = b.health {
            if h < 80.0 {
                s.push(sg(
                    Cat::Bateria,
                    Sev::Info,
                    &format!("Bateria com {:.0}% da capacidade original", h),
                    "A bateria já está desgastada, o que reduz a autonomia independentemente de qualquer ajuste. \
                     Trocar a bateria costuma ser o maior ganho nesse caso."
                        .into(),
                    None,
                ));
            }
        }
        s.push(sg(
            Cat::Bateria,
            Sev::Info,
            "Limite a carga em 80% se usa na tomada",
            "Manter a bateria entre 20% e 80% desacelera o desgaste. Funciona apenas em notebooks compatíveis \
             (ThinkPad, alguns ASUS, Dell, etc.) e exige o TLP."
                .into(),
            Some(format!("sudo tlp setcharge 20 80 {bat_name}")),
        ));
        if r.has_nvidia {
            let cmd = if r.distro_id == "pop" {
                "sudo system76-power graphics integrated".to_string()
            } else {
                "pipx install envycontrol && sudo envycontrol -s integrated".to_string()
            };
            s.push(sg(
                Cat::Bateria,
                Sev::Aviso,
                "Use a GPU integrada no dia a dia",
                "Há uma GPU NVIDIA no sistema. Mantê-la ligada o tempo todo pode custar vários watts. \
                 Deixe só a GPU integrada na bateria e use a NVIDIA apenas para jogos/renderização. Requer reiniciar."
                    .into(),
                Some(cmd),
            ));
        }
        if is_intel && has("powertop") == false {
            s.push(sg(
                Cat::Bateria,
                Sev::Info,
                "Meça e ajuste o consumo com o PowerTOP",
                "O PowerTOP mostra o que mais gasta energia e aplica ajustes automáticos em CPU Intel. \
                 Se usar o TLP, rode apenas para diagnóstico."
                    .into(),
                Some(format!("{} && sudo powertop", install_cmd(pm, "powertop"))),
            ));
        }
        if let Some(iface) = wifi_iface() {
            if has("iw") {
                let ps = run("iw", &["dev", &iface, "get", "power_save"]);
                if ps.to_lowercase().contains("off") {
                    s.push(sg(
                        Cat::Bateria,
                        Sev::Aviso,
                        "Ative a economia de energia do Wi-Fi",
                        format!(
                            "O adaptador {iface} está com power save desligado. Ligá-lo reduz o consumo do Wi-Fi com \
                             impacto mínimo na velocidade para uso comum."
                        ),
                        Some(format!("sudo iw dev {iface} set power_save on")),
                    ));
                }
            }
        }
    }

    // ── Memória ──
    if let Some(sw) = rd(Path::new("/proc/sys/vm/swappiness")).and_then(|v| v.parse::<u32>().ok()) {
        if sw > 30 && r.ram_gb >= 6.0 {
            s.push(sg(
                Cat::Memoria,
                Sev::Info,
                "Reduza o uso da swap",
                format!(
                    "Com {:.0} GB de RAM e swappiness em {sw}, o sistema usa o disco antes do necessário. \
                     Valor 10 mantém mais coisas na RAM (mais rápido e poupa o SSD).",
                    r.ram_gb
                ),
                Some("echo 'vm.swappiness=10' | sudo tee /etc/sysctl.d/99-swappiness.conf && sudo sysctl --system".into()),
            ));
        }
    }
    let has_zram = Path::new("/sys/block/zram0").exists();
    if !has_zram && r.ram_gb <= 8.5 {
        let cmd = if pm == "apt" {
            "sudo apt install -y zram-tools".to_string()
        } else {
            format!(
                "{} && printf '[zram0]\\nzram-size = ram / 2\\n' | sudo tee /etc/systemd/zram-generator.conf && sudo systemctl daemon-reload && sudo systemctl start systemd-zram-setup@zram0.service",
                install_cmd(pm, "zram-generator")
            )
        };
        s.push(sg(
            Cat::Memoria,
            Sev::Aviso,
            "Ative a ZRAM (swap comprimida na RAM)",
            "Com pouca RAM, a ZRAM comprime páginas de memória em vez de ir para o disco. Deixa o sistema \
             mais fluido e reduz escritas no SSD."
                .into(),
            Some(cmd),
        ));
    }
    if r.ram_used_pct > 85.0 {
        let hog = r.top_mem.first().map(|t| t.0.clone()).unwrap_or_default();
        s.push(sg(
            Cat::Memoria,
            Sev::Aviso,
            "Memória quase cheia",
            format!(
                "{:.0}% da RAM está em uso. O maior consumidor agora é “{hog}”. Feche abas/apps pesados \
                 ou considere um ambiente mais leve.",
                r.ram_used_pct
            ),
            None,
        ));
    }
    if r.ram_gb < 4.0 {
        s.push(sg(
            Cat::Memoria,
            Sev::Importante,
            "Pouca RAM: use um ambiente gráfico leve",
            format!(
                "Seu PC tem {:.1} GB de RAM. XFCE, LXQt ou i3 usam bem menos que GNOME/KDE. \
                 Veja a aba “Distro ideal” para opções leves.",
                r.ram_gb
            ),
            None,
        ));
    }

    // ── Disco ──
    if r.disks.iter().any(|d| d.ssd) && !enabled("fstrim.timer") {
        s.push(sg(
            Cat::Disco,
            Sev::Aviso,
            "Ative o TRIM semanal do SSD",
            "O TRIM avisa o SSD quais blocos estão livres, mantendo a velocidade e a vida útil. Está desativado."
                .into(),
            Some("sudo systemctl enable --now fstrim.timer".into()),
        ));
    }
    let hdds: Vec<&DiskInfo> = r.disks.iter().filter(|d| !d.ssd).collect();
    if !hdds.is_empty() {
        let nomes: Vec<String> = hdds.iter().map(|d| format!("{} ({:.0} GB)", d.name, d.size_gb)).collect();
        let tem_ssd = r.disks.iter().any(|d| d.ssd);
        s.push(sg(
            Cat::Disco,
            if tem_ssd { Sev::Info } else { Sev::Importante },
            "HDD detectado",
            format!(
                "{}. {}",
                nomes.join(", "),
                if tem_ssd {
                    "Instale o sistema e os programas no SSD e deixe o HDD para arquivos grandes."
                } else {
                    "Trocar por um SSD é o upgrade que mais acelera um PC antigo: boot, apps e atualizações ficam muito mais rápidos."
                }
            ),
            None,
        ));
    }
    if let Some(u) = r.root_used_pct {
        if u > 85.0 {
            let clean = match pm {
                "apt" => "sudo apt autoremove --purge -y && sudo apt clean",
                "dnf" => "sudo dnf autoremove -y && sudo dnf clean all",
                "pacman" => "sudo pacman -Sc",
                "zypper" => "sudo zypper clean --all",
                _ => "true",
            };
            s.push(sg(
                Cat::Disco,
                Sev::Importante,
                "Disco principal quase cheio",
                format!(
                    "A partição raiz está {:.0}% ocupada. Disco cheio deixa tudo lento (e SSD cheio perde desempenho). \
                     Este comando limpa pacotes antigos e logs.",
                    u
                ),
                Some(format!("{clean} && sudo journalctl --vacuum-size=200M")),
            ));
        }
    }

    // ── CPU ──
    if let Some((name, cpu, _)) = r.top_cpu.first() {
        if *cpu > 20.0 {
            s.push(sg(
                Cat::Cpu,
                Sev::Aviso,
                &format!("“{name}” está usando {:.0}% da CPU", cpu),
                "Um único programa consumindo tanto CPU aumenta calor, ruído do ventilador e gasto de bateria. \
                 Feche-o, atualize-o ou investigue se é um processo travado."
                    .into(),
                None,
            ));
        }
    }
    if r.cpu_usage > 40.0 {
        s.push(sg(
            Cat::Cpu,
            Sev::Aviso,
            "CPU ocupada mesmo sem você pedir",
            format!(
                "O uso médio durante a análise foi de {:.0}%. Veja a aba “Processos” para descobrir quem está \
                 consumindo.",
                r.cpu_usage
            ),
            None,
        ));
    }
    if is_intel && !has("thermald") {
        s.push(sg(
            Cat::Cpu,
            Sev::Info,
            "Instale o thermald (CPUs Intel)",
            "Controla a temperatura de forma inteligente, evitando quedas de desempenho por superaquecimento."
                .into(),
            Some(format!("{} && sudo systemctl enable --now thermald", install_cmd(pm, "thermald"))),
        ));
    }
    let candidatos = [
        ("cups.service", "cups"),
        ("cups-browsed.service", "cups-browsed"),
        ("ModemManager.service", "ModemManager"),
        ("avahi-daemon.service", "avahi-daemon"),
    ];
    let ativos: Vec<&str> = candidatos.iter().filter(|(u, _)| enabled(u)).map(|(_, n)| *n).collect();
    if !ativos.is_empty() {
        s.push(sg(
            Cat::Cpu,
            Sev::Info,
            "Desative serviços que talvez você não use",
            format!(
                "Ativos: {}. Impressão (cups), modem celular (ModemManager) e descoberta de rede (avahi) rodam em \
                 segundo plano. Desative só se você não usa impressora, modem 3G/4G nem AirPlay/Chromecast.",
                ativos.join(", ")
            ),
            Some(format!("sudo systemctl disable --now {}", ativos.join(" "))),
        ));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let n = fs::read_dir(Path::new(&home).join(".config/autostart"))
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        if n > 8 {
            s.push(sg(
                Cat::Sistema,
                Sev::Aviso,
                &format!("{n} programas abrem junto com o sistema"),
                "Cada app na inicialização deixa o login mais lento e consome RAM/CPU. Desative os que você não \
                 precisa nas configurações de “Aplicativos de inicialização”."
                    .into(),
                Some("ls ~/.config/autostart".into()),
            ));
        }
    }

    // ── Sistema ──
    if let Some(b) = r.boot_secs {
        if b > 45.0 {
            s.push(sg(
                Cat::Sistema,
                Sev::Aviso,
                &format!("Boot demorado ({:.0} s)", b),
                "Veja quais serviços atrasam a inicialização e desative os desnecessários.".into(),
                Some("systemd-analyze blame | head -15".into()),
            ));
        }
    }

    if s.is_empty() {
        s.push(sg(
            Cat::Sistema,
            Sev::Info,
            "Tudo certo!",
            "Não encontrei nada importante para ajustar neste momento.".into(),
            None,
        ));
    }
    s.sort_by(|a, b| b.sev.cmp(&a.sev));
    let pen: i32 = s
        .iter()
        .map(|x| match x.sev {
            Sev::Importante => 12,
            Sev::Aviso => 5,
            Sev::Info => 1,
        })
        .sum();
    r.score = (100 - pen).clamp(5, 100) as u8;
    r.suggestions = s;
    r
}

// ───────────────────────────── Distros ─────────────────────────────

struct Distro {
    name: &'static str,
    ids: &'static [&'static str],
    tagline: &'static str,
    ease: f32,
    bat: f32,
    light: f32,
    fresh: f32,
    nvidia: f32,
    min_ram: f64,
}

static DISTROS: [Distro; 8] = [
    Distro { name: "Linux Mint (Cinnamon ou Xfce)", ids: &["linuxmint"], tagline: "Simples, estável e familiar para quem vem do Windows.", ease: 9.5, bat: 7.0, light: 7.0, fresh: 5.0, nvidia: 8.0, min_ram: 2.0 },
    Distro { name: "Fedora Workstation", ids: &["fedora"], tagline: "Kernel e drivers novos, ótima gestão de energia, muito polido.", ease: 7.0, bat: 8.5, light: 5.0, fresh: 9.0, nvidia: 6.0, min_ram: 4.0 },
    Distro { name: "Ubuntu LTS", ids: &["ubuntu"], tagline: "Suporte longo, enorme comunidade e compatibilidade com tudo.", ease: 8.5, bat: 7.0, light: 4.0, fresh: 5.0, nvidia: 8.5, min_ram: 4.0 },
    Distro { name: "Pop!_OS", ids: &["pop"], tagline: "Ótimo em notebooks, troca de GPU fácil e NVIDIA sem dor de cabeça.", ease: 8.0, bat: 8.0, light: 5.0, fresh: 6.0, nvidia: 10.0, min_ram: 4.0 },
    Distro { name: "Debian", ids: &["debian"], tagline: "Rocha de estabilidade e consumo baixo de recursos.", ease: 6.0, bat: 7.0, light: 8.0, fresh: 3.0, nvidia: 6.0, min_ram: 1.0 },
    Distro { name: "EndeavourOS (Arch)", ids: &["endeavouros", "arch"], tagline: "Arch com instalador amigável: sempre atualizado e totalmente customizável.", ease: 4.0, bat: 8.0, light: 9.0, fresh: 10.0, nvidia: 7.0, min_ram: 2.0 },
    Distro { name: "Xubuntu / Lubuntu", ids: &["xubuntu", "lubuntu"], tagline: "Base Ubuntu com ambiente leve: revive PCs antigos.", ease: 8.0, bat: 7.0, light: 9.5, fresh: 5.0, nvidia: 7.5, min_ram: 1.0 },
    Distro { name: "MX Linux (Xfce)", ids: &["mx"], tagline: "Leve, rápida e com ótimas ferramentas próprias para configurar tudo.", ease: 7.5, bat: 7.0, light: 9.0, fresh: 4.0, nvidia: 7.0, min_ram: 1.0 },
];

#[derive(PartialEq, Clone, Copy)]
enum Goal {
    Autonomia,
    Desempenho,
    Facilidade,
}

#[derive(PartialEq, Clone, Copy)]
enum Exp {
    Iniciante,
    Intermediario,
    Avancado,
}

fn rank(r: &Report, goal: Goal, exp: Exp) -> Vec<(&'static Distro, f32)> {
    let mut w_ease = match exp {
        Exp::Iniciante => 3.0,
        Exp::Intermediario => 1.5,
        Exp::Avancado => 0.5,
    };
    let (mut w_bat, mut w_light, mut w_fresh, mut w_nv) = (0.5f32, 0.5f32, 0.5f32, 0.0f32);
    match goal {
        Goal::Autonomia => {
            w_bat += 3.0;
            w_light += 1.0;
        }
        Goal::Desempenho => {
            w_light += 2.0;
            w_fresh += 1.5;
        }
        Goal::Facilidade => w_ease += 1.5,
    }
    if r.ram_gb < 4.0 {
        w_light += 3.0;
    } else if r.ram_gb < 8.0 {
        w_light += 1.5;
    }
    if !r.disks.is_empty() && r.disks.iter().all(|d| !d.ssd) {
        w_light += 1.0;
    }
    if r.battery.is_some() {
        w_bat += 1.0;
    }
    if r.has_nvidia {
        w_nv += 1.5;
    }
    let total = (w_ease + w_bat + w_light + w_fresh + w_nv) * 10.0;
    let mut v: Vec<(&'static Distro, f32)> = DISTROS
        .iter()
        .map(|d| {
            let mut s = d.ease * w_ease + d.bat * w_bat + d.light * w_light + d.fresh * w_fresh + d.nvidia * w_nv;
            if d.min_ram > r.ram_gb + 0.5 {
                s *= 0.5;
            }
            (d, s / total * 100.0)
        })
        .collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v
}

fn reasons(d: &Distro, r: &Report, goal: Goal, exp: Exp) -> Vec<String> {
    let mut v = vec![];
    if r.ram_gb < 8.0 && d.light >= 8.0 {
        v.push(format!("Roda bem com os seus {:.0} GB de RAM", r.ram_gb.round()));
    }
    if r.battery.is_some() && d.bat >= 8.0 {
        v.push("Boa gestão de energia para notebook".into());
    }
    if r.has_nvidia && d.nvidia >= 8.0 {
        v.push("Bom suporte à sua GPU NVIDIA".into());
    }
    if exp == Exp::Iniciante && d.ease >= 8.0 {
        v.push("Fácil para quem está começando".into());
    }
    if goal == Goal::Desempenho && d.fresh >= 9.0 {
        v.push("Kernel e drivers sempre atualizados".into());
    }
    if d.min_ram > r.ram_gb + 0.5 {
        v.push(format!("Atenção: recomendado ao menos {:.0} GB de RAM", d.min_ram));
    }
    v
}

// ───────────────────────────── Interface ─────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Resumo,
    Sugestoes,
    Processos,
    Distro,
}

struct App {
    tab: Tab,
    report: Option<Rc<Report>>,
    rx: Option<Receiver<Report>>,
    filter: Option<Cat>,
    goal: Goal,
    exp: Exp,
    toast: Option<Instant>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let mut app = App {
            tab: Tab::Resumo,
            report: None,
            rx: None,
            filter: None,
            goal: Goal::Autonomia,
            exp: Exp::Intermediario,
            toast: None,
        };
        app.start_scan(ctx);
        app
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(scan());
            ctx.request_repaint();
        });
        self.rx = Some(rx);
    }

    fn copy(&mut self, ui: &egui::Ui, text: String) {
        ui.ctx().copy_text(text);
        self.toast = Some(Instant::now());
    }

    fn tab_resumo(&mut self, ui: &mut egui::Ui, r: &Report) {
        ui.horizontal(|ui| {
            gauge(ui, r.score);
            ui.vertical(|ui| {
                let (msg, color) = match r.score {
                    85..=100 => ("Seu PC está ótimo!", Color32::from_rgb(90, 200, 120)),
                    60..=84 => ("Dá para melhorar bastante", Color32::from_rgb(240, 190, 70)),
                    _ => ("Seu PC precisa de atenção", Color32::from_rgb(240, 100, 100)),
                };
                ui.add_space(20.0);
                ui.label(RichText::new(msg).size(26.0).strong().color(color));
                let imp = r.suggestions.iter().filter(|s| s.sev == Sev::Importante).count();
                ui.label(format!(
                    "{} sugestões encontradas, {} importantes.",
                    r.suggestions.len(),
                    imp
                ));
                if ui.button("Ver todas as sugestões").clicked() {
                    self.tab = Tab::Sugestoes;
                }
            });
        });
        ui.add_space(8.0);
        ui.separator();
        ui.heading("Seu computador");
        ui.horizontal_wrapped(|ui| {
            card(ui, "Processador", vec![r.cpu_model.clone(), format!("{} núcleos, uso agora: {:.0}%", r.cores, r.cpu_usage)]);
            card(ui, "Memória", vec![format!("{:.1} GB de RAM ({:.0}% em uso)", r.ram_gb, r.ram_used_pct), format!("Swap: {:.1} GB", r.swap_gb)]);
            let mut disk_lines: Vec<String> = r
                .disks
                .iter()
                .map(|d| format!("{}: {} {:.0} GB", d.name, if d.ssd { "SSD" } else { "HDD" }, d.size_gb))
                .collect();
            if let Some(u) = r.root_used_pct {
                disk_lines.push(format!("Disco do sistema {:.0}% cheio", u));
            }
            card(ui, "Armazenamento", disk_lines);
            let bat = match &r.battery {
                Some(b) => {
                    let mut l = vec![format!("{}% ({})", b.pct.unwrap_or(0), b.status)];
                    if let Some(h) = b.health {
                        l.push(format!("Saúde: {:.0}%", h));
                    }
                    if let Some(w) = b.watts {
                        if w > 0.1 {
                            l.push(format!("Consumo: {:.1} W", w));
                        }
                    }
                    l
                }
                None => vec!["Sem bateria (desktop)".into()],
            };
            card(ui, "Bateria", bat);
            card(ui, "Vídeo", vec![r.gpu.clone()]);
            let mut sis = vec![r.distro.clone(), format!("Kernel {}", r.kernel), format!("Desktop: {}", r.desktop)];
            if let Some(b) = r.boot_secs {
                sis.push(format!("Boot: {:.0} s", b));
            }
            card(ui, "Sistema", sis);
        });
        ui.add_space(8.0);
        ui.heading("Primeiras coisas a fazer");
        for s in r.suggestions.iter().take(3) {
            ui.horizontal(|ui| {
                ui.label(RichText::new(s.sev.label()).color(s.sev.color()).strong());
                ui.label(&s.title);
            });
        }
    }

    fn tab_sugestoes(&mut self, ui: &mut egui::Ui, r: &Report) {
        ui.heading("Sugestões para o seu PC");
        ui.label("Nada é aplicado automaticamente. Copie o comando e cole no terminal quando quiser.");
        let mut copy_all = false;
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.filter, None, "Todas");
            for c in Cat::ALL {
                ui.selectable_value(&mut self.filter, Some(c), c.label());
            }
            if ui.button("Copiar todos os comandos").clicked() {
                copy_all = true;
            }
        });
        if copy_all {
            let all = r
                .suggestions
                .iter()
                .filter_map(|s| s.cmd.as_ref().map(|c| format!("# {}\n{}", s.title, c)))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.copy(ui, all);
        }
        ui.add_space(6.0);
        let filter = self.filter;
        for s in r.suggestions.iter().filter(|s| filter.map_or(true, |c| c == s.cat)) {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(s.sev.label()).color(s.sev.color()).strong());
                    ui.label(RichText::new(s.cat.label()).weak());
                });
                ui.label(RichText::new(&s.title).size(18.0).strong());
                ui.label(&s.why);
                if let Some(cmd) = &s.cmd {
                    ui.add_space(4.0);
                    ui.code(cmd);
                    if ui.button("Copiar comando").clicked() {
                        self.copy(ui, cmd.clone());
                    }
                }
            });
        }
    }

    fn tab_processos(&mut self, ui: &mut egui::Ui, r: &Report) {
        ui.heading("Quem está consumindo recursos");
        ui.label("Processos com o mesmo nome (ex.: abas do navegador) são somados.");
        ui.add_space(6.0);
        ui.columns(2, |cols| {
            cols[0].label(RichText::new("Mais CPU").strong());
            egui::Grid::new("cpu_grid").striped(true).show(&mut cols[0], |ui| {
                for (n, c, m) in &r.top_cpu {
                    ui.label(n);
                    ui.label(format!("{:.1}%", c));
                    ui.label(format!("{} MB", m / 1024 / 1024));
                    ui.end_row();
                }
            });
            cols[1].label(RichText::new("Mais memória").strong());
            egui::Grid::new("mem_grid").striped(true).show(&mut cols[1], |ui| {
                for (n, c, m) in &r.top_mem {
                    ui.label(n);
                    ui.label(format!("{} MB", m / 1024 / 1024));
                    ui.label(format!("{:.1}%", c));
                    ui.end_row();
                }
            });
        });
    }

    fn tab_distro(&mut self, ui: &mut egui::Ui, r: &Report) {
        ui.heading("Qual distro combina com este PC?");
        ui.label("Responda duas perguntas rápidas. O resultado também considera RAM, disco, bateria e GPU detectados.");
        ui.add_space(6.0);
        ui.label(RichText::new("O que é mais importante para você?").strong());
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.goal, Goal::Autonomia, "Bateria durar mais");
            ui.selectable_value(&mut self.goal, Goal::Desempenho, "Desempenho máximo");
            ui.selectable_value(&mut self.goal, Goal::Facilidade, "Facilidade de uso");
        });
        ui.label(RichText::new("Seu nível com Linux").strong());
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.exp, Exp::Iniciante, "Iniciante");
            ui.selectable_value(&mut self.exp, Exp::Intermediario, "Intermediário");
            ui.selectable_value(&mut self.exp, Exp::Avancado, "Avançado");
        });
        ui.add_space(6.0);
        ui.separator();
        ui.label(format!("Você usa agora: {}", r.distro));
        for (i, (d, pct)) in rank(r, self.goal, self.exp).iter().take(5).enumerate() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}. {}", i + 1, d.name)).size(19.0).strong());
                    if i == 0 {
                        ui.label(RichText::new("MELHOR ESCOLHA").color(Color32::from_rgb(90, 200, 120)).strong());
                    }
                    if d.ids.contains(&r.distro_id.as_str()) {
                        ui.label(RichText::new("você já usa").weak());
                    }
                });
                ui.add(egui::ProgressBar::new(pct / 100.0).text(format!("{:.0}% de compatibilidade", pct)));
                ui.label(d.tagline);
                for why in reasons(d, r, self.goal, self.exp) {
                    ui.label(format!("• {why}"));
                }
            });
        }
    }
}

fn gauge(ui: &mut egui::Ui, score: u8) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(150.0, 150.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let c = rect.center();
    let radius = 62.0;
    let color = match score {
        85..=100 => Color32::from_rgb(90, 200, 120),
        60..=84 => Color32::from_rgb(240, 190, 70),
        _ => Color32::from_rgb(240, 100, 100),
    };
    p.circle_stroke(c, radius, egui::Stroke::new(11.0, Color32::from_gray(55)));
    let steps = 72;
    let n = (steps as f32 * score as f32 / 100.0) as i32;
    let pts: Vec<egui::Pos2> = (0..=n)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + i as f32 / steps as f32 * std::f32::consts::TAU;
            c + egui::vec2(a.cos(), a.sin()) * radius
        })
        .collect();
    if pts.len() > 1 {
        p.add(egui::Shape::line(pts, egui::Stroke::new(11.0, color)));
    }
    p.text(
        c,
        egui::Align2::CENTER_CENTER,
        score.to_string(),
        egui::FontId::proportional(40.0),
        ui.visuals().strong_text_color(),
    );
}

fn card(ui: &mut egui::Ui, title: &str, lines: Vec<String>) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_width(250.0);
        ui.set_max_width(300.0);
        ui.label(RichText::new(title).strong().size(17.0));
        for l in lines {
            ui.label(l);
        }
    });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut got = None;
        if let Some(rx) = &self.rx {
            got = rx.try_recv().ok();
        }
        if let Some(r) = got {
            self.report = Some(Rc::new(r));
            self.rx = None;
        }
        if self.rx.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let busy = self.rx.is_some();
        let report = self.report.clone();

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Otimizador Linux");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if busy { "Analisando..." } else { "Analisar meu PC" };
                    if ui.add_enabled(!busy, egui::Button::new(RichText::new(label).size(16.0))).clicked() {
                        self.start_scan(ctx);
                    }
                    if let Some(t) = self.toast {
                        if t.elapsed() < Duration::from_secs(2) {
                            ui.label(RichText::new("Copiado!").color(Color32::from_rgb(90, 200, 120)));
                            ctx.request_repaint_after(Duration::from_millis(500));
                        }
                    }
                });
            });
            ui.add_space(6.0);
        });

        egui::SidePanel::left("nav").resizable(false).exact_width(190.0).show(ctx, |ui| {
            ui.add_space(10.0);
            let n = report.as_ref().map(|r| r.suggestions.len()).unwrap_or(0);
            let items = [
                (Tab::Resumo, "Resumo".to_string()),
                (Tab::Sugestoes, format!("Sugestões ({n})")),
                (Tab::Processos, "Processos".to_string()),
                (Tab::Distro, "Distro ideal".to_string()),
            ];
            for (t, label) in items {
                ui.selectable_value(&mut self.tab, t, RichText::new(label).size(17.0));
                ui.add_space(4.0);
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| match &report {
            None => {
                ui.vertical_centered(|ui| {
                    ui.add_space(120.0);
                    if busy {
                        ui.spinner();
                        ui.label(RichText::new("Analisando seu PC...").size(20.0));
                    } else {
                        ui.label("Clique em “Analisar meu PC”.");
                    }
                });
            }
            Some(r) => {
                if busy {
                    ui.label("Reanalisando...");
                }
                egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| match self.tab {
                    Tab::Resumo => self.tab_resumo(ui, r),
                    Tab::Sugestoes => self.tab_sugestoes(ui, r),
                    Tab::Processos => self.tab_processos(ui, r),
                    Tab::Distro => self.tab_distro(ui, r),
                });
            }
        });
    }
}

fn setup_style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(10.0, 8.0);
        s.spacing.button_padding = egui::vec2(14.0, 7.0);
        s.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
        s.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(16.0));
        s.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(26.0));
        s.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(14.0));
    });
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 700.0])
            .with_min_inner_size([780.0, 520.0])
            .with_title("Otimizador Linux"),
        ..Default::default()
    };
    eframe::run_native(
        "Otimizador Linux",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(App::new(&cc.egui_ctx)))
        }),
    )
}
