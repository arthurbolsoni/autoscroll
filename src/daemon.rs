// Daemon: dá grab em cada mouse, reemite tudo por um espelho uinput com a
// mesma identidade (nome, vendor, product), e enquanto o botão do meio está
// segurado prende o cursor no ponto do clique e injeta roda conforme a
// distância arrastada.

use evdev::uinput::VirtualDevice;
use evdev::{
    AttributeSet, Device, EventType, InputEvent, KeyCode, RelativeAxisCode,
};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

static DEBUG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

macro_rules! log {
    ($($t:tt)*) => { eprintln!($($t)*) };
}
macro_rules! debug {
    ($($t:tt)*) => {
        if DEBUG.load(Ordering::Relaxed) { eprintln!($($t)*) }
    };
}
const BTN_MOUSE: u16 = 0x110;
const BTN_JOYSTICK: u16 = 0x120;

// Marca dos espelhos no campo version do input_id (with_phys do evdev dá
// EINVAL). O KDE guarda as configurações do mouse por vendor/product/nome,
// não pela versão, então o espelho continua herdando tudo.
const MARCA_VERSAO: u16 = 0xa5c0;
const CONFIG_PADRAO: &str = "/etc/autoscroll.conf";
pub const SOCK_DIR: &str = "/run/autoscroll";
pub const SOCK_PATH: &str = "/run/autoscroll/state.sock";
const HIRES_POR_DENTE: f64 = 120.0;
const MAX_TICK_DT: f64 = 0.25;
const GHOST_HZ: f64 = 60.0;
const MAX_CLIENTES: usize = 16;
const MAX_LINHA: usize = 4096;
const MAX_FOCO: usize = 256;
const MAX_GRAB: usize = 16;

// Shells do desktop: rolar em cima deles troca área de trabalho no KDE.
const SHELLS: &[&str] = &[
    "plasmashell", "nemo-desktop", "xfdesktop", "waybar", "xfce4-panel",
    "org.gnome.shell", "gjs",
];

#[derive(Clone, Debug)]
struct Config {
    deadzone_px: f64,
    speed_mult: f64,
    speed_exp: f64,
    max_px_per_sec: f64,
    px_per_notch: f64,
    max_drag_px: f64,
    tick_hz: f64,
    natural: bool,
    toggle_mode: bool,
    desktop_scroll: bool,
    ghost_cursor: bool,
    ghost_scale: f64,
    snap_cursor_on_release: bool,
    anchor_cursor: bool,
    blacklist: Vec<String>,
    extra_devices: Vec<String>,
    ignore_devices: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        // Curva do Chromium/Edge: 0.000008 * d^2.2 px/ms, zona morta 15 px.
        Config {
            deadzone_px: 15.0,
            speed_mult: 0.008,
            speed_exp: 2.2,
            max_px_per_sec: 30000.0,
            px_per_notch: 55.0,
            max_drag_px: 1200.0,
            tick_hz: 90.0,
            natural: false,
            toggle_mode: false,
            desktop_scroll: false,
            ghost_cursor: true,
            ghost_scale: 1.0,
            snap_cursor_on_release: true,
            anchor_cursor: false,
            blacklist: vec!["freecad".into(), "orcaslicer".into(), "minecraft".into()],
            extra_devices: vec![],
            ignore_devices: vec![],
        }
    }
}

fn lista(v: &str) -> Vec<String> {
    v.split(',')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect()
}

fn bool_de(v: &str) -> bool {
    matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn ler_config(caminho: &Path) -> Config {
    let mut c = Config::default();
    let Ok(meta) = std::fs::symlink_metadata(caminho) else {
        return c;
    };
    // Arquivo decide quais dispositivos um processo root abre: só vale se
    // for do root e ninguém mais puder escrever.
    if !meta.is_file() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        log!("config: ignorando {}: precisa ser do root e sem escrita de grupo/outros", caminho.display());
        return c;
    }
    let Ok(texto) = std::fs::read_to_string(caminho) else {
        return c;
    };
    for linha in texto.lines() {
        let linha = linha.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = linha.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let num = |alvo: &mut f64, positivo: bool| match v.parse::<f64>() {
            Ok(x) if x.is_finite() && x >= 0.0 && (!positivo || x > 0.0) => *alvo = x,
            _ => log!("config: valor inválido para {k}: {v:?}"),
        };
        match k {
            "DEADZONE_PX" => num(&mut c.deadzone_px, false),
            "SPEED_MULT" => num(&mut c.speed_mult, true),
            "SPEED_EXP" => num(&mut c.speed_exp, true),
            "MAX_PX_PER_SEC" => num(&mut c.max_px_per_sec, true),
            "PX_PER_NOTCH" => num(&mut c.px_per_notch, true),
            "MAX_DRAG_PX" => num(&mut c.max_drag_px, true),
            "TICK_HZ" => num(&mut c.tick_hz, true),
            "GHOST_SCALE" => num(&mut c.ghost_scale, true),
            "NATURAL" => c.natural = bool_de(v),
            "TOGGLE_MODE" => c.toggle_mode = bool_de(v),
            "DESKTOP_SCROLL" => c.desktop_scroll = bool_de(v),
            "GHOST_CURSOR" => c.ghost_cursor = bool_de(v),
            "SNAP_CURSOR_ON_RELEASE" => c.snap_cursor_on_release = bool_de(v),
            "ANCHOR_CURSOR" => c.anchor_cursor = bool_de(v),
            "BLACKLIST" => c.blacklist = lista(v),
            "EXTRA_DEVICES" => c.extra_devices = lista(v),
            "IGNORE_DEVICES" => c.ignore_devices = lista(v),
            _ => log!("config: chave desconhecida {k}"),
        }
    }
    c
}

// ---- foco e socket de estado -------------------------------------------

#[derive(Default)]
struct Foco {
    por_cliente: HashMap<u64, String>,
}

impl Foco {
    fn bloqueado(&self, cfg: &Config) -> bool {
        self.por_cliente.values().any(|classe| {
            let c = classe.to_lowercase();
            cfg.blacklist.iter().any(|b| c.contains(b))
                || (!cfg.desktop_scroll && SHELLS.iter().any(|b| c.contains(b)))
        })
    }
}

struct Cliente {
    stream: UnixStream,
    uid: u32,
}

#[derive(Default)]
struct Notificador {
    clientes: HashMap<u64, Cliente>,
    msg: &'static str,
    ultima_pos: Option<(i32, i32)>,
    uids_ativos: HashSet<u32>,
}

fn estado_uid(uid: u32) -> String {
    std::fs::read_to_string(format!("/run/systemd/users/{uid}"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("STATE=").map(str::to_string))
        })
        .unwrap_or_default()
}

fn uid_do_par(s: &UnixStream) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}

impl Notificador {
    fn enviar(&mut self, msg: &str, so_ativos: bool) {
        let mut mortos = vec![];
        for (id, c) in self.clientes.iter_mut() {
            if so_ativos && !self.uids_ativos.contains(&c.uid) {
                continue;
            }
            match c.stream.write(msg.as_bytes()) {
                Ok(_) => {}
                // cliente atrasado: pula esta atualização
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => mortos.push(*id),
            }
        }
        for id in mortos {
            self.clientes.remove(&id);
        }
    }

    fn set(&mut self, ativo: bool) {
        let msg = if ativo { "1\n" } else { "0\n" };
        if msg == self.msg {
            return;
        }
        self.msg = msg;
        self.ultima_pos = None;
        if ativo {
            self.uids_ativos = self
                .clientes
                .values()
                .map(|c| c.uid)
                .filter(|u| estado_uid(*u) == "active")
                .collect();
        }
        self.enviar(msg, false);
    }

    fn pos(&mut self, dx: f64, dy: f64) {
        if self.msg != "1\n" {
            return;
        }
        let p = (dx as i32, dy as i32);
        if self.ultima_pos == Some(p) {
            return;
        }
        self.ultima_pos = Some(p);
        self.enviar(&format!("pos {} {}\n", p.0, p.1), true);
    }
}

struct Global {
    cfg: Config,
    foco: Mutex<Foco>,
    notif: Mutex<Notificador>,
    estados: Mutex<HashMap<PathBuf, Arc<Mutex<Estado>>>>,
}

fn servidor(g: Arc<Global>) {
    let _ = std::fs::create_dir_all(SOCK_DIR);
    let _ = std::fs::remove_file(SOCK_PATH);
    let ouvinte = match UnixListener::bind(SOCK_PATH) {
        Ok(o) => o,
        Err(e) => {
            log!("socket do overlay indisponível: {e}");
            return;
        }
    };
    let _ = std::fs::set_permissions(SOCK_PATH, std::fs::Permissions::from_mode(0o666));
    let proximo = AtomicU64::new(1);
    for conexao in ouvinte.incoming() {
        let Ok(stream) = conexao else { continue };
        let Some(uid) = uid_do_par(&stream) else { continue };
        if !matches!(estado_uid(uid).as_str(), "active" | "online") {
            debug!("socket recusado do uid {uid}");
            continue;
        }
        let id = proximo.fetch_add(1, Ordering::Relaxed);
        {
            let mut n = g.notif.lock().unwrap();
            let do_uid = n.clientes.values().filter(|c| c.uid == uid).count();
            if n.clientes.len() >= MAX_CLIENTES || do_uid >= 4 {
                log!("clientes demais no socket; recusando uid {uid}");
                continue;
            }
            let Ok(escrita) = stream.try_clone() else { continue };
            let _ = escrita.set_nonblocking(true);
            let mut escrita2 = escrita.try_clone().ok();
            if let Some(e) = escrita2.as_mut() {
                let _ = e.write(n.msg.as_bytes());
            }
            if n.uids_ativos.is_empty() || estado_uid(uid) == "active" {
                n.uids_ativos.insert(uid);
            }
            n.clientes.insert(id, Cliente { stream: escrita, uid });
        }
        let g = g.clone();
        thread::spawn(move || {
            let mut leitor = BufReader::new(stream);
            let mut linha = String::new();
            loop {
                linha.clear();
                match std::io::Read::take(&mut leitor, MAX_LINHA as u64).read_line(&mut linha) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if !linha.ends_with('\n') && linha.len() >= MAX_LINHA {
                    break;
                }
                if let Some(classe) = linha.trim().strip_prefix("focus ") {
                    let limpa: String = classe
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(MAX_FOCO)
                        .collect();
                    let mut f = g.foco.lock().unwrap();
                    f.por_cliente.insert(id, limpa.clone());
                    debug!("foco: {limpa:?} (bloqueado={})", f.bloqueado(&g.cfg));
                }
            }
            g.foco.lock().unwrap().por_cliente.remove(&id);
            g.notif.lock().unwrap().clientes.remove(&id);
        });
    }
}

// ---- estado por mouse ----------------------------------------------------

struct Estado {
    ui: VirtualDevice,
    pendente: bool,  // meio segurado, zona morta ainda não vencida
    ativo: bool,     // rolando por arrasto
    alternado: bool, // modo alternar rolando (botão solto)
    repasse: bool,   // meio segurado sobre app da lista negra
    comer_soltura: Option<u16>,
    dx: f64,
    dy: f64,
    acc_v: f64,
    acc_h: f64,
    dente_v: f64,
    dente_h: f64,
}

impl Estado {
    fn zerar(&mut self) {
        self.pendente = false;
        self.ativo = false;
        self.alternado = false;
        self.repasse = false;
        self.comer_soltura = None;
        self.dx = 0.0;
        self.dy = 0.0;
        self.acc_v = 0.0;
        self.acc_h = 0.0;
        self.dente_v = 0.0;
        self.dente_h = 0.0;
    }

    fn rolando(&self) -> bool {
        self.ativo || self.alternado
    }

    fn acumular(&mut self, cod: u16, v: i32, max: f64) {
        // Com o cursor preso nada limita o arrasto; limita como a borda da
        // tela limita no Windows.
        let lim = |x: f64| x.signum() * x.abs().min(max);
        if cod == RelativeAxisCode::REL_X.0 {
            self.dx = lim(self.dx + v as f64);
        } else {
            self.dy = lim(self.dy + v as f64);
        }
    }

    fn emitir(&mut self, evs: &[InputEvent]) {
        if let Err(e) = self.ui.emit(evs) {
            debug!("emit: {e}");
        }
    }
}

fn ev(tipo: EventType, cod: u16, v: i32) -> InputEvent {
    InputEvent::new(tipo.0, cod, v)
}

fn velocidade_px(cfg: &Config, offset: f64) -> f64 {
    if offset.abs() <= cfg.deadzone_px {
        return 0.0;
    }
    let v = cfg.speed_mult * offset.abs().powf(cfg.speed_exp);
    offset.signum() * v.min(cfg.max_px_per_sec)
}

fn tick(st: &mut Estado, dt: f64, cfg: &Config, bloqueado: bool) {
    if bloqueado {
        // App da lista negra em foco: nunca rola, e para o que estiver rolando.
        if st.rolando() || st.pendente {
            st.zerar();
        }
        return;
    }
    if !cfg.toggle_mode
        && st.pendente
        && (st.dx.abs() > cfg.deadzone_px || st.dy.abs() > cfg.deadzone_px)
    {
        st.pendente = false;
        st.ativo = true;
        debug!("rolagem começou");
    }
    if !st.rolando() {
        return;
    }
    let s = if cfg.natural { 1.0 } else { -1.0 };
    let para_hires = HIRES_POR_DENTE / cfg.px_per_notch;
    // Arrastar para baixo = roda para baixo (REL_WHEEL negativo); para a
    // direita = REL_HWHEEL positivo.
    st.acc_v += s * velocidade_px(cfg, st.dy) * para_hires * dt;
    st.acc_h += -s * velocidade_px(cfg, st.dx) * para_hires * dt;
    let mut evs = Vec::with_capacity(4);
    let iv = st.acc_v.trunc();
    if iv != 0.0 {
        st.acc_v -= iv;
        st.dente_v += iv;
        evs.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_WHEEL_HI_RES.0, iv as i32));
        let n = (st.dente_v / HIRES_POR_DENTE).trunc();
        if n != 0.0 {
            st.dente_v -= n * HIRES_POR_DENTE;
            evs.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_WHEEL.0, n as i32));
        }
    }
    let ih = st.acc_h.trunc();
    if ih != 0.0 {
        st.acc_h -= ih;
        st.dente_h += ih;
        evs.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_HWHEEL_HI_RES.0, ih as i32));
        let n = (st.dente_h / HIRES_POR_DENTE).trunc();
        if n != 0.0 {
            st.dente_h -= n * HIRES_POR_DENTE;
            evs.push(ev(EventType::RELATIVE, RelativeAxisCode::REL_HWHEEL.0, n as i32));
        }
    }
    if !evs.is_empty() {
        st.emitir(&evs);
    }
}

fn relogio(g: Arc<Global>) {
    let passo = Duration::from_secs_f64(1.0 / g.cfg.tick_hz);
    let mut ultimo = Instant::now();
    let mut ultimo_ghost = ultimo;
    loop {
        thread::sleep(passo);
        let agora = Instant::now();
        // Mede o tempo real entre ticks: sob carga o sleep atrasa, e usar o
        // passo nominal deixaria a rolagem mais lenta que o configurado.
        let dt = (agora - ultimo).as_secs_f64().min(MAX_TICK_DT);
        ultimo = agora;
        let bloqueado = g.foco.lock().unwrap().bloqueado(&g.cfg);
        let estados: Vec<_> = g.estados.lock().unwrap().values().cloned().collect();
        let mut algum = false;
        let mut arrasto = None;
        for st in &estados {
            let mut st = st.lock().unwrap();
            tick(&mut st, dt, &g.cfg, bloqueado);
            algum |= st.rolando();
            if st.ativo && arrasto.is_none() {
                arrasto = Some((st.dx, st.dy));
            }
        }
        let mut n = g.notif.lock().unwrap();
        n.set(algum);
        if g.cfg.anchor_cursor && g.cfg.ghost_cursor && (agora - ultimo_ghost).as_secs_f64() >= 1.0 / GHOST_HZ {
            ultimo_ghost = agora;
            // Só o arrasto preso tem fantasma: no modo alternar o cursor de
            // verdade anda.
            if let Some((dx, dy)) = arrasto {
                n.pos(dx * g.cfg.ghost_scale, dy * g.cfg.ghost_scale);
            }
        }
    }
}

fn eh_botao(cod: u16) -> bool {
    (BTN_MOUSE..BTN_JOYSTICK).contains(&cod)
}

// Modo alternar (Explorer/Firefox): um clique do meio começa, qualquer
// clique para. Devolve true se o evento foi consumido.
fn tecla_alternar(e: &InputEvent, st: &mut Estado, bloqueado: bool) -> bool {
    let (cod, v) = (e.code(), e.value());
    if v == 0 && st.comer_soltura == Some(cod) {
        st.comer_soltura = None;
        return true;
    }
    if st.alternado {
        if v == 1 {
            debug!("rolagem alternada parou");
            st.zerar();
            st.comer_soltura = Some(cod);
        }
        return true;
    }
    if cod == KeyCode::BTN_MIDDLE.0 {
        if v == 1 {
            if bloqueado {
                st.repasse = true;
                st.emitir(&[ev(EventType::KEY, cod, 1)]);
            } else {
                st.pendente = true;
            }
        } else if v == 0 {
            if st.repasse {
                st.repasse = false;
                st.emitir(&[ev(EventType::KEY, cod, 0)]);
            } else if st.pendente {
                st.zerar();
                st.alternado = true;
                debug!("rolagem alternada começou");
            }
        }
        return true;
    }
    false
}

fn criar_espelho(dev: &Device) -> std::io::Result<VirtualDevice> {
    let mut teclas = AttributeSet::<KeyCode>::new();
    if let Some(k) = dev.supported_keys() {
        for t in k.iter() {
            teclas.insert(t);
        }
    }
    for c in BTN_MOUSE..BTN_JOYSTICK {
        teclas.insert(KeyCode(c));
    }
    let mut rel = AttributeSet::<RelativeAxisCode>::new();
    if let Some(r) = dev.supported_relative_axes() {
        for a in r.iter() {
            rel.insert(a);
        }
    }
    for a in [
        RelativeAxisCode::REL_X,
        RelativeAxisCode::REL_Y,
        RelativeAxisCode::REL_WHEEL,
        RelativeAxisCode::REL_HWHEEL,
        RelativeAxisCode::REL_WHEEL_HI_RES,
        RelativeAxisCode::REL_HWHEEL_HI_RES,
    ] {
        rel.insert(a);
    }
    // Mesmo nome e ids do mouse de verdade: o KDE aplica ao espelho as
    // configurações que já tem para esse mouse (velocidade, aceleração).
    let id = dev.input_id();
    VirtualDevice::builder()?
        .name(dev.name().unwrap_or("mouse"))
        .input_id(evdev::InputId::new(id.bus_type(), id.vendor(), id.product(), MARCA_VERSAO))
        .with_keys(&teclas)?
        .with_relative_axes(&rel)?
        .build()
}

fn bombear(caminho: PathBuf, mut dev: Device, g: Arc<Global>) {
    let nome = dev.name().unwrap_or("?").to_string();
    if let Err(e) = dev.grab() {
        log!("sem grab em {}: {e}", caminho.display());
        return;
    }
    let ui = match criar_espelho(&dev) {
        Ok(u) => u,
        Err(e) => {
            log!("sem espelho para {nome}: {e}");
            return;
        }
    };
    let st = Arc::new(Mutex::new(Estado {
        ui,
        pendente: false,
        ativo: false,
        alternado: false,
        repasse: false,
        comer_soltura: None,
        dx: 0.0,
        dy: 0.0,
        acc_v: 0.0,
        acc_h: 0.0,
        dente_v: 0.0,
        dente_h: 0.0,
    }));
    g.estados.lock().unwrap().insert(caminho.clone(), st.clone());
    log!("grab em {nome} ({})", caminho.display());
    let cfg = &g.cfg;
    let mut seguros: HashSet<u16> = HashSet::new();
    'fora: loop {
        let lote: Vec<InputEvent> = match dev.fetch_events() {
            Ok(it) => it.collect(),
            Err(_) => break 'fora, // desconectado
        };
        let bloqueado = g.foco.lock().unwrap().bloqueado(cfg);
        let mut st = st.lock().unwrap();
        let mut saida: Vec<InputEvent> = Vec::with_capacity(lote.len());
        for e in lote {
            let (tipo, cod, v) = (e.event_type(), e.code(), e.value());
            if cfg.toggle_mode && tipo == EventType::KEY && eh_botao(cod) {
                if !saida.is_empty() {
                    st.emitir(&saida);
                    saida.clear();
                }
                if tecla_alternar(&e, &mut st, bloqueado) {
                    continue;
                }
            } else if tipo == EventType::KEY && cod == KeyCode::BTN_MIDDLE.0 {
                if !saida.is_empty() {
                    st.emitir(&saida);
                    saida.clear();
                }
                if v == 1 {
                    if bloqueado {
                        // App que usa o arrasto do meio: repassa o botão.
                        st.repasse = true;
                        st.emitir(&[ev(EventType::KEY, cod, 1)]);
                    } else {
                        st.zerar();
                        st.pendente = true;
                    }
                } else if v == 0 {
                    if st.repasse {
                        st.repasse = false;
                        st.emitir(&[ev(EventType::KEY, cod, 0)]);
                    } else {
                        let (clique, dx, dy) = (st.pendente, st.dx, st.dy);
                        st.zerar();
                        if cfg.anchor_cursor && cfg.snap_cursor_on_release && (dx as i32 != 0 || dy as i32 != 0) {
                            // Leva o cursor preso até onde a mão foi, como o Windows.
                            st.emitir(&[
                                ev(EventType::RELATIVE, RelativeAxisCode::REL_X.0, dx as i32),
                                ev(EventType::RELATIVE, RelativeAxisCode::REL_Y.0, dy as i32),
                            ]);
                        }
                        if clique {
                            st.emitir(&[ev(EventType::KEY, cod, 1)]);
                            st.emitir(&[ev(EventType::KEY, cod, 0)]);
                        }
                    }
                }
                continue;
            }
            if tipo == EventType::RELATIVE
                && (cod == RelativeAxisCode::REL_X.0 || cod == RelativeAxisCode::REL_Y.0)
            {
                if st.alternado {
                    // Modo alternar: mede a distância mas deixa o cursor andar.
                    st.acumular(cod, v, cfg.max_drag_px);
                } else if st.pendente || st.ativo {
                    st.acumular(cod, v, cfg.max_drag_px);
                    // ANCHOR_CURSOR (midscroll): engole o movimento para o
                    // cursor ficar preso; sem ele o cursor anda junto.
                    if cfg.anchor_cursor {
                        continue;
                    }
                }
            }
            match tipo {
                EventType::SYNCHRONIZATION => {}
                EventType::KEY | EventType::RELATIVE => {
                    if tipo == EventType::KEY {
                        if v == 1 {
                            seguros.insert(cod);
                        } else if v == 0 {
                            seguros.remove(&cod);
                        }
                    }
                    saida.push(e);
                }
                _ => {}
            }
        }
        if !saida.is_empty() {
            st.emitir(&saida);
        }
    }
    // Não deixa botão preso no espelho se o mouse sumiu no meio do clique.
    let mut st = st.lock().unwrap();
    let mut soltar: Vec<InputEvent> = seguros
        .iter()
        .map(|c| ev(EventType::KEY, *c, 0))
        .collect();
    if st.repasse {
        soltar.push(ev(EventType::KEY, KeyCode::BTN_MIDDLE.0, 0));
    }
    if !soltar.is_empty() {
        st.emitir(&soltar);
    }
    st.zerar();
    drop(st);
    g.estados.lock().unwrap().remove(&caminho);
    log!("solto {}", caminho.display());
}

// ---- escolha de dispositivos ---------------------------------------------

fn eh_mouse(dev: &Device) -> bool {
    let rel = dev.supported_relative_axes();
    let keys = dev.supported_keys();
    let abs = dev.supported_absolute_axes();
    keys.is_some_and(|k| k.contains(KeyCode::BTN_MIDDLE))
        && rel.is_some_and(|r| {
            r.contains(RelativeAxisCode::REL_X) && r.contains(RelativeAxisCode::REL_Y)
        })
        && !abs.is_some_and(|a| {
            a.contains(evdev::AbsoluteAxisCode::ABS_X)
                || a.contains(evdev::AbsoluteAxisCode::ABS_MT_POSITION_X)
        })
}

fn eh_teclado(dev: &Device) -> bool {
    dev.supported_keys().is_some_and(|k| {
        [KeyCode::KEY_A, KeyCode::KEY_Z, KeyCode::KEY_SPACE, KeyCode::KEY_ENTER]
            .iter()
            .all(|t| k.contains(*t))
    })
}

fn casa(dev: &Device, caminho: &Path, spec: &str) -> bool {
    if spec.starts_with('/') {
        return std::fs::canonicalize(spec).is_ok_and(|p| p == caminho);
    }
    if let Some((v, p)) = spec.split_once(':')
        && let (Ok(v), Ok(p)) = (u16::from_str_radix(v, 16), u16::from_str_radix(p, 16))
    {
        let id = dev.input_id();
        return id.vendor() == v && id.product() == p;
    }
    spec.len() >= 3 && dev.name().unwrap_or("").to_lowercase().contains(spec)
}

fn decidir(dev: &Device, caminho: &Path, cfg: &Config) -> (bool, String) {
    if dev.input_id().version() == MARCA_VERSAO {
        return (false, "espelho do próprio autoscroll".into());
    }
    if eh_teclado(dev) {
        return (false, "teclado".into());
    }
    if let Some(s) = cfg.ignore_devices.iter().find(|s| casa(dev, caminho, s)) {
        return (false, format!("ignorado por {s:?}"));
    }
    if let Some(s) = cfg.extra_devices.iter().find(|s| casa(dev, caminho, s)) {
        return (true, format!("forçado por {s:?}"));
    }
    if eh_mouse(dev) {
        return (true, "mouse".into());
    }
    (false, "não é mouse".into())
}

fn listar(cfg: &Config) {
    let mut devs: Vec<_> = evdev::enumerate().collect();
    devs.sort_by_key(|(p, _)| {
        p.to_string_lossy()
            .trim_start_matches("/dev/input/event")
            .parse::<u32>()
            .unwrap_or(0)
    });
    for (p, d) in devs {
        let id = d.input_id();
        let (usa, motivo) = decidir(&d, &p, cfg);
        println!(
            "{}  {:04x}:{:04x}  {}\n    {}: {motivo}\n",
            p.display(),
            id.vendor(),
            id.product(),
            d.name().unwrap_or(""),
            if usa { "usado" } else { "não usado" }
        );
    }
}

pub fn main(args: Vec<String>) {
    let mut config = PathBuf::from(CONFIG_PADRAO);
    let mut so_listar = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--debug" => DEBUG.store(true, Ordering::Relaxed),
            "--list-devices" => so_listar = true,
            "--config" => config = it.next().map(PathBuf::from).unwrap_or(config),
            _ => {
                eprintln!("argumento desconhecido: {a}");
                std::process::exit(2);
            }
        }
    }
    let cfg = ler_config(&config);
    if so_listar {
        listar(&cfg);
        return;
    }
    debug!("config: {cfg:?}");
    let g = Arc::new(Global {
        cfg,
        foco: Mutex::default(),
        notif: Mutex::new(Notificador { msg: "0\n", ..Default::default() }),
        estados: Mutex::default(),
    });
    {
        let g = g.clone();
        thread::spawn(move || servidor(g));
    }
    {
        let g = g.clone();
        thread::spawn(move || relogio(g));
    }
    log!("rodando");
    // Hotplug: só examina caminhos nunca vistos; um caminho que some é
    // esquecido, então religar o mouse faz ele ser examinado de novo.
    let vistos: RwLock<HashSet<PathBuf>> = RwLock::default();
    loop {
        let atuais: HashSet<PathBuf> = std::fs::read_dir("/dev/input")
            .map(|d| {
                d.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with("event"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        vistos.write().unwrap().retain(|p| atuais.contains(p));
        let mut novos: Vec<_> = atuais
            .iter()
            .filter(|p| !vistos.read().unwrap().contains(*p))
            .cloned()
            .collect();
        novos.sort();
        for caminho in novos {
            vistos.write().unwrap().insert(caminho.clone());
            let Ok(dev) = Device::open(&caminho) else { continue };
            let (usa, motivo) = decidir(&dev, &caminho, &g.cfg);
            debug!("{} ({}): {motivo}", dev.name().unwrap_or(""), caminho.display());
            if !usa {
                continue;
            }
            if g.estados.lock().unwrap().len() >= MAX_GRAB {
                log!("já com {MAX_GRAB} dispositivos; pulando {}", caminho.display());
                continue;
            }
            let g = g.clone();
            thread::spawn(move || bombear(caminho, dev, g));
        }
        thread::sleep(Duration::from_secs(2));
    }
}
