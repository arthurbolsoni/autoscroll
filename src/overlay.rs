// Overlay da sessão: enquanto o arrasto está ativo desenha o selo com as
// setas no ponto onde o cursor ficou preso e um cursor fantasma que segue a
// mão; informa ao daemon a classe da janela em foco (lista negra).
//
// Desenho via wlr-layer-shell, duas superfícies pequenas na camada overlay
// sem região de entrada (cliques passam direto). Posição do ponto e janela
// em foco vêm do kdotool.

use crate::daemon::SOCK_PATH;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

const SELO_PX: i32 = 42;
const ICONE_PX: f32 = 24.0;
const GHOST_ALFA: f32 = 0.85;
const FOCO_SEG: u64 = 1;

enum Msg {
    Ativo(bool),
    Pos(i32, i32),
    Ancora(i32, i32),
    Desconectado,
}

fn kdotool() -> Command {
    // cargo install deixa em ~/.cargo/bin, que o serviço não tem no PATH
    let local = std::env::var("HOME")
        .map(|h| format!("{h}/.cargo/bin/kdotool"))
        .unwrap_or_default();
    if std::path::Path::new(&local).exists() {
        Command::new(local)
    } else {
        Command::new("kdotool")
    }
}

fn classe_ativa() -> String {
    kdotool()
        .args(["getactivewindow", "getwindowclassname"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn posicao_mouse() -> Option<(i32, i32)> {
    let o = kdotool().args(["getmouselocation", "--shell"]).output().ok()?;
    let t = String::from_utf8_lossy(&o.stdout);
    let pega = |k: &str| {
        t.lines()
            .find_map(|l| l.strip_prefix(k))
            .and_then(|v| v.trim().parse().ok())
    };
    Some((pega("X=")?, pega("Y=")?))
}

fn socket_confiavel() -> bool {
    // /run/autoscroll é do root; socket de outro dono = algo errado.
    std::fs::metadata(SOCK_PATH).is_ok_and(|m| {
        use std::os::unix::fs::FileTypeExt;
        m.file_type().is_socket() && (m.uid() == 0 || m.uid() == 65534)
    })
}

fn vigiar_socket(tx: mpsc::Sender<Msg>, acorda: OwnedFd) {
    let avisa = |tx: &mpsc::Sender<Msg>, m: Msg| {
        let _ = tx.send(m);
        unsafe { libc::write(acorda.as_raw_fd(), [1u8].as_ptr() as *const _, 1) };
    };
    loop {
        if socket_confiavel()
            && let Ok(s) = UnixStream::connect(SOCK_PATH)
        {
            let mut escrita = s.try_clone().unwrap();
            let (para, parar) = mpsc::channel::<()>();
            // Thread que informa a janela em foco quando ela muda.
            thread::spawn(move || {
                let mut ultima = None;
                loop {
                    let c = classe_ativa();
                    if ultima.as_ref() != Some(&c) {
                        if writeln!(escrita, "focus {c}").is_err() {
                            return;
                        }
                        ultima = Some(c);
                    }
                    if parar.recv_timeout(Duration::from_secs(FOCO_SEG))
                        != Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        return;
                    }
                }
            });
            for linha in BufReader::new(s).lines() {
                let Ok(l) = linha else { break };
                match l.trim() {
                    "1" => {
                        avisa(&tx, Msg::Ativo(true));
                        // O cursor fica preso o arrasto todo: uma consulta basta.
                        let (tx, acorda) = (tx.clone(), acorda.try_clone().unwrap());
                        thread::spawn(move || {
                            if let Some((x, y)) = posicao_mouse() {
                                let _ = tx.send(Msg::Ancora(x, y));
                                unsafe {
                                    libc::write(acorda.as_raw_fd(), [1u8].as_ptr() as *const _, 1)
                                };
                            }
                        });
                    }
                    "0" => avisa(&tx, Msg::Ativo(false)),
                    l => {
                        let p: Vec<_> = l.split_whitespace().collect();
                        if let ["pos", x, y] = p[..]
                            && let (Ok(x), Ok(y)) = (x.parse::<i32>(), y.parse::<i32>())
                            && x.abs() <= 100_000
                            && y.abs() <= 100_000
                        {
                            avisa(&tx, Msg::Pos(x, y));
                        }
                    }
                }
            }
            drop(para);
            avisa(&tx, Msg::Desconectado);
        }
        thread::sleep(Duration::from_secs(2));
    }
}

// ---- desenho -------------------------------------------------------------

struct Imagem {
    w: i32,
    h: i32,
    px: Vec<u32>, // ARGB pré-multiplicado
}

fn misturar(dst: &mut u32, r: f32, g: f32, b: f32, a: f32) {
    // "over" com cor não pré-multiplicada sobre destino pré-multiplicado
    let da = ((*dst >> 24) & 255) as f32 / 255.0;
    let dr = ((*dst >> 16) & 255) as f32 / 255.0;
    let dg = ((*dst >> 8) & 255) as f32 / 255.0;
    let db = (*dst & 255) as f32 / 255.0;
    let oa = a + da * (1.0 - a);
    let or = r * a + dr * (1.0 - a);
    let og = g * a + dg * (1.0 - a);
    let ob = b * a + db * (1.0 - a);
    let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    *dst = (c(oa) << 24) | (c(or) << 16) | (c(og) << 8) | c(ob);
}

fn dist_segmento(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let t = (((px - a.0) * vx + (py - a.1) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
    let (qx, qy) = (a.0 + t * vx - px, a.1 + t * vy - py);
    (qx * qx + qy * qy).sqrt()
}

// Selo do midscroll: círculo escuro com borda clara e o ícone lucide
// move-vertical (seta para cima e para baixo).
fn desenhar_selo() -> Imagem {
    let n = SELO_PX;
    let mut px = vec![0u32; (n * n) as usize];
    let c = n as f32 / 2.0;
    let r = c - 0.5;
    let esc = ICONE_PX / 24.0;
    let o = c - ICONE_PX / 2.0;
    let p = |x: f32, y: f32| (o + x * esc, o + y * esc);
    let segs = [
        (p(12.0, 2.0), p(12.0, 22.0)),
        (p(8.0, 18.0), p(12.0, 22.0)),
        (p(12.0, 22.0), p(16.0, 18.0)),
        (p(8.0, 6.0), p(12.0, 2.0)),
        (p(12.0, 2.0), p(16.0, 6.0)),
    ];
    let meia = esc; // traço de 2 px no ícone de 24
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - c).powi(2) + (fy - c).powi(2)).sqrt();
            let dst = &mut px[(y * n + x) as usize];
            let dentro = (r - d + 0.5).clamp(0.0, 1.0);
            if dentro > 0.0 {
                misturar(dst, 30.0 / 255.0, 30.0 / 255.0, 32.0 / 255.0, 0.82 * dentro);
                let borda = (1.0 - (d - (r - 0.5)).abs()).clamp(0.0, 1.0) * dentro;
                if borda > 0.0 {
                    misturar(dst, 1.0, 1.0, 1.0, 0.28 * borda);
                }
            }
            let ds = segs
                .iter()
                .map(|(a, b)| dist_segmento(fx, fy, *a, *b))
                .fold(f32::MAX, f32::min);
            let cob = (meia - ds + 0.5).clamp(0.0, 1.0);
            if cob > 0.0 {
                misturar(dst, 0xe8 as f32 / 255.0, 0xe8 as f32 / 255.0, 0xe8 as f32 / 255.0, cob);
            }
        }
    }
    Imagem { w: n, h: n, px }
}

fn ler_kcminputrc(chave: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let t = std::fs::read_to_string(format!("{home}/.config/kcminputrc")).ok()?;
    t.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == chave && !v.trim().is_empty()).then(|| v.trim().to_string())
    })
}

fn achar_cursor(tema: &str, vistos: &mut Vec<String>) -> Option<std::path::PathBuf> {
    if tema.is_empty() || vistos.iter().any(|v| v == tema) || vistos.len() > 8 {
        return None;
    }
    vistos.push(tema.to_string());
    let home = std::env::var("HOME").unwrap_or_default();
    let dirs = [
        format!("{home}/.local/share/icons"),
        format!("{home}/.icons"),
        "/usr/share/icons".into(),
        "/usr/local/share/icons".into(),
        "/usr/share/pixmaps".into(),
    ];
    let mut herda = vec![];
    for d in &dirs {
        let base = std::path::Path::new(d).join(tema);
        for nome in ["default", "left_ptr", "arrow", "top_left_arrow"] {
            let p = base.join("cursors").join(nome);
            if p.is_file() {
                return Some(p);
            }
        }
        if let Ok(t) = std::fs::read_to_string(base.join("index.theme")) {
            for l in t.lines().filter(|l| l.starts_with("Inherits")) {
                if let Some((_, v)) = l.split_once('=') {
                    herda.extend(v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
                }
            }
        }
    }
    herda.iter().find_map(|t| achar_cursor(t, vistos))
}

// Lê a imagem do tamanho mais próximo de um arquivo Xcursor (primeiro quadro).
fn ler_xcursor(caminho: &std::path::Path, quer: u32) -> Option<(Imagem, i32, i32)> {
    let d = std::fs::read(caminho).ok()?;
    let u = |o: usize| -> Option<u32> { Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?)) };
    if d.get(..4)? != b"Xcur" {
        return None;
    }
    let (cab, ntoc) = (u(4)? as usize, u(12)?);
    let mut melhor: Option<(u32, usize)> = None;
    for i in 0..ntoc.min(1024) as usize {
        let base = cab + i * 12;
        let (tipo, nominal, pos) = (u(base)?, u(base + 4)?, u(base + 8)? as usize);
        if tipo != 0xfffd0002 {
            continue;
        }
        if melhor.is_none_or(|(n, _)| nominal.abs_diff(quer) < n.abs_diff(quer)) {
            melhor = Some((nominal, pos));
        }
    }
    let (_, pos) = melhor?;
    let (w, h, xh, yh) = (u(pos + 16)?, u(pos + 20)?, u(pos + 24)?, u(pos + 28)?);
    if !(1..=256).contains(&w) || !(1..=256).contains(&h) || xh > w || yh > h {
        return None;
    }
    let px = (0..(w * h) as usize)
        .map(|i| u(pos + 36 + i * 4))
        .collect::<Option<Vec<u32>>>()?;
    // aplica a transparência do fantasma (pixels já pré-multiplicados)
    let px = px
        .into_iter()
        .map(|p| {
            let f = |s: u32| ((((p >> s) & 255) as f32 * GHOST_ALFA) as u32) << s;
            f(24) | f(16) | f(8) | f(0)
        })
        .collect();
    Some((Imagem { w: w as i32, h: h as i32, px }, xh as i32, yh as i32))
}

// Seta simples para quando o tema não pôde ser lido.
fn seta_reserva() -> (Imagem, i32, i32) {
    let (w, h) = (22, 34);
    let pts = [(1.0, 1.0), (1.0, 25.0), (6.5, 20.0), (10.0, 28.5), (13.5, 27.0), (10.0, 19.0), (17.0, 18.5)];
    let mut px = vec![0u32; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // ponto dentro do polígono (par-ímpar)
            let mut dentro = false;
            for i in 0..pts.len() {
                let (a, b): ((f32, f32), (f32, f32)) = (pts[i], pts[(i + 1) % pts.len()]);
                if (a.1 > fy) != (b.1 > fy) && fx < (b.0 - a.0) * (fy - a.1) / (b.1 - a.1) + a.0 {
                    dentro = !dentro;
                }
            }
            let borda = (0..pts.len())
                .map(|i| dist_segmento(fx, fy, pts[i], pts[(i + 1) % pts.len()]))
                .fold(f32::MAX, f32::min);
            let dst = &mut px[(y * w + x) as usize];
            if dentro {
                misturar(dst, 1.0, 1.0, 1.0, 0.72);
            }
            let cob = (0.75 - borda + 0.5).clamp(0.0, 1.0);
            if cob > 0.0 {
                misturar(dst, 0.0, 0.0, 0.0, 0.85 * cob);
            }
        }
    }
    (Imagem { w, h, px }, 1, 1)
}

fn carregar_fantasma() -> (Imagem, i32, i32) {
    let tema = std::env::var("XCURSOR_THEME")
        .ok()
        .or_else(|| ler_kcminputrc("cursorTheme"))
        .unwrap_or_else(|| "default".into());
    let tam = std::env::var("XCURSOR_SIZE")
        .ok()
        .or_else(|| ler_kcminputrc("cursorSize"))
        .and_then(|v| v.parse().ok())
        .filter(|v| (1..=256).contains(v))
        .unwrap_or(24);
    achar_cursor(&tema, &mut vec![])
        .and_then(|p| ler_xcursor(&p, tam))
        .unwrap_or_else(seta_reserva)
}

// ---- wayland -------------------------------------------------------------

struct Saida {
    out: wl_output::WlOutput,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

struct Camada {
    surf: wl_surface::WlSurface,
    camada: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    buf: Option<wl_buffer::WlBuffer>,
    configurada: bool,
}

#[derive(Default)]
struct App {
    comp: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    saidas: Vec<Saida>,
    selo: Option<Camada>,
    fantasma: Option<Camada>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => app.comp = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => app.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => app.shell = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_output" => {
                    let out = reg.bind(name, version.min(2), qh, ());
                    app.saidas.push(Saida { out, x: 0, y: 0, w: 0, h: 0 });
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for App {
    fn event(
        app: &mut Self,
        out: &wl_output::WlOutput,
        ev: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(s) = app.saidas.iter_mut().find(|s| &s.out == out) else { return };
        match ev {
            wl_output::Event::Geometry { x, y, .. } => {
                s.x = x;
                s.y = y;
            }
            wl_output::Event::Mode { flags: WEnum::Value(f), width, height, .. }
                if f.contains(wl_output::Mode::Current) =>
            {
                s.w = width;
                s.h = height;
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        app: &mut Self,
        ls: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        ev: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                ls.ack_configure(serial);
                for c in [app.selo.as_mut(), app.fantasma.as_mut()].into_iter().flatten() {
                    if &c.camada == ls && !c.configurada {
                        c.configurada = true;
                        c.surf.attach(c.buf.as_ref(), 0, 0);
                        c.surf.damage_buffer(0, 0, i32::MAX, i32::MAX);
                        c.surf.commit();
                    }
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                app.esconder();
            }
            _ => {}
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_region::WlRegion);
delegate_noop!(App: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

impl App {
    fn buffer(&self, img: &Imagem, qh: &QueueHandle<Self>) -> Option<wl_buffer::WlBuffer> {
        let tam = (img.w * img.h * 4) as usize;
        let fd = unsafe { libc::memfd_create(c"autoscroll".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut arq = std::fs::File::from(fd.try_clone().ok()?);
        let bytes: Vec<u8> = img.px.iter().flat_map(|p| p.to_le_bytes()).collect();
        arq.write_all(&bytes).ok()?;
        let pool = self.shm.as_ref()?.create_pool(fd.as_fd(), tam as i32, qh, ());
        let buf = pool.create_buffer(0, img.w, img.h, img.w * 4, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();
        Some(buf)
    }

    fn criar(&self, img: &Imagem, saida: Option<&wl_output::WlOutput>, x: i32, y: i32, qh: &QueueHandle<Self>) -> Option<Camada> {
        let comp = self.comp.as_ref()?;
        let surf = comp.create_surface(qh, ());
        // Região de entrada vazia: nada nesta superfície recebe clique.
        let reg = comp.create_region(qh, ());
        surf.set_input_region(Some(&reg));
        reg.destroy();
        let camada = self.shell.as_ref()?.get_layer_surface(
            &surf,
            saida,
            zwlr_layer_shell_v1::Layer::Overlay,
            "autoscroll".into(),
            qh,
            (),
        );
        camada.set_size(img.w as u32, img.h as u32);
        camada.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
        camada.set_exclusive_zone(-1);
        camada.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        camada.set_margin(y.max(0), 0, 0, x.max(0));
        surf.commit();
        Some(Camada { surf, camada, buf: self.buffer(img, qh), configurada: false })
    }

    fn esconder(&mut self) {
        for c in [self.selo.take(), self.fantasma.take()].into_iter().flatten() {
            c.camada.destroy();
            c.surf.destroy();
            if let Some(b) = c.buf {
                b.destroy();
            }
        }
    }
}

fn mover(c: &Camada, x: i32, y: i32) {
    c.camada.set_margin(y.max(0), 0, 0, x.max(0));
    c.surf.commit();
}

pub fn main() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("autoscroll overlay: sessão sem Wayland; só informa o foco");
    }
    let mut pipe = [0i32; 2];
    unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    let (leitura, escrita) = unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || vigiar_socket(tx, escrita));

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("autoscroll overlay: sem Wayland ({e}); só informa o foco");
            loop {
                thread::park();
            }
        }
    };
    let mut fila = conn.new_event_queue();
    let qh = fila.handle();
    conn.display().get_registry(&qh, ());
    let mut app = App::default();
    let _ = fila.roundtrip(&mut app);
    let _ = fila.roundtrip(&mut app);
    if app.shell.is_none() {
        eprintln!("autoscroll overlay: compositor sem wlr-layer-shell; nada será desenhado");
    }
    let selo = desenhar_selo();
    let (fantasma, hx, hy) = carregar_fantasma();
    eprintln!("autoscroll overlay: fantasma {}x{} (ponta {hx},{hy})", fantasma.w, fantasma.h);

    let mut ativo = false;
    let mut ancora: Option<(i32, i32, i32, i32)> = None; // x, y locais + w, h da saída
    let mut saida_ancora: Option<wl_output::WlOutput>;
    let mut offset = (0, 0);

    loop {
        let _ = fila.flush();
        if let Some(guarda) = fila.prepare_read() {
            let mut fds = [
                libc::pollfd { fd: guarda.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: leitura.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            unsafe { libc::poll(fds.as_mut_ptr(), 2, 1000) };
            if fds[0].revents & libc::POLLIN != 0 {
                let _ = guarda.read();
            } else {
                drop(guarda);
            }
            let mut lixo = [0u8; 64];
            while unsafe { libc::read(leitura.as_raw_fd(), lixo.as_mut_ptr() as *mut _, 64) } > 0 {}
        }
        if fila.dispatch_pending(&mut app).is_err() {
            eprintln!("autoscroll overlay: conexão Wayland caiu");
            std::process::exit(1);
        }
        let mut mudou = false;
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Ativo(a) => {
                    if a != ativo {
                        ativo = a;
                        if !a {
                            app.esconder();
                            ancora = None;
                            offset = (0, 0);
                        }
                    }
                }
                Msg::Desconectado => {
                    ativo = false;
                    app.esconder();
                    ancora = None;
                }
                Msg::Pos(x, y) => {
                    offset = (x, y);
                    mudou = true;
                }
                Msg::Ancora(x, y) => {
                    if !ativo || ancora.is_some() {
                        continue;
                    }
                    let saida = app.saidas.iter().find(|o| {
                        o.w > 0 && x >= o.x && x < o.x + o.w && y >= o.y && y < o.y + o.h
                    });
                    let (ax, ay, w, h) = match saida {
                        Some(o) => (x - o.x, y - o.y, o.w, o.h),
                        None => (x, y, i32::MAX, i32::MAX),
                    };
                    saida_ancora = saida.map(|o| o.out.clone());
                    ancora = Some((ax, ay, w, h));
                    app.selo = app.criar(&selo, saida_ancora.as_ref(), ax - SELO_PX / 2, ay - SELO_PX / 2, &qh);
                    app.fantasma = app.criar(&fantasma, saida_ancora.as_ref(), ax - hx, ay - hy, &qh);
                    mudou = true;
                }
            }
        }
        if mudou
            && let (Some((ax, ay, w, h)), Some(f)) = (ancora, app.fantasma.as_ref())
        {
            // O Windows para o cursor na borda da tela; aqui também.
            let gx = (ax + offset.0).clamp(0, w - 1);
            let gy = (ay + offset.1).clamp(0, h - 1);
            if f.configurada {
                mover(f, gx - hx, gy - hy);
            } else {
                f.camada.set_margin((gy - hy).max(0), 0, 0, (gx - hx).max(0));
            }
        }
    }
}
