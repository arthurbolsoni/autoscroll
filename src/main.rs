// Autoscroll estilo Windows: segurar o botão do meio e afastar o mouse rola a
// janela sob o cursor, com velocidade proporcional à distância do ponto de
// clique. O cursor continua se movendo. Clique do meio sem arrastar continua
// sendo clique do meio (colar).
//
// Lê cada mouse com grab exclusivo e reemite tudo por um dispositivo uinput.

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, Device, EventType, InputEvent, Key, RelativeAxisType};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const PREFIXO_VIRTUAL: &str = "autoscroll";
// Distância (em contagens do mouse) para o clique virar rolagem.
const LIMIAR: i32 = 12;
// Zona morta em volta do ponto de clique onde não rola.
const ZONA_MORTA: f64 = 15.0;
const TICK: Duration = Duration::from_millis(8);

#[derive(Default)]
struct Estado {
    segurando: bool,
    rolando: bool,
    dx: i32,
    dy: i32,
    resto_v: f64,
    resto_h: f64,
    notch_v: i32,
    notch_h: i32,
}

fn velocidade() -> f64 {
    // unidades hi-res (120 = 1 dente da roda) por segundo, por contagem além da zona morta
    std::env::var("AUTOSCROLL_VELOCIDADE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8.0)
}

fn eh_mouse(dev: &Device) -> bool {
    let nome = dev.name().unwrap_or("");
    if nome.starts_with(PREFIXO_VIRTUAL) || nome.contains("keyd virtual") {
        return false;
    }
    let rel_ok = dev.supported_relative_axes().is_some_and(|r| {
        r.contains(RelativeAxisType::REL_X) && r.contains(RelativeAxisType::REL_Y)
    });
    let btn_ok = dev
        .supported_keys()
        .is_some_and(|k| k.contains(Key::BTN_MIDDLE));
    rel_ok && btn_ok
}

fn criar_virtual(dev: &Device) -> std::io::Result<VirtualDevice> {
    let mut rel = AttributeSet::<RelativeAxisType>::new();
    if let Some(r) = dev.supported_relative_axes() {
        for a in r.iter() {
            rel.insert(a);
        }
    }
    for a in [
        RelativeAxisType::REL_WHEEL,
        RelativeAxisType::REL_HWHEEL,
        RelativeAxisType::REL_WHEEL_HI_RES,
        RelativeAxisType::REL_HWHEEL_HI_RES,
    ] {
        rel.insert(a);
    }
    let mut teclas = AttributeSet::<Key>::new();
    if let Some(k) = dev.supported_keys() {
        for t in k.iter() {
            teclas.insert(t);
        }
    }
    let nome = format!("{PREFIXO_VIRTUAL} {}", dev.name().unwrap_or("mouse"));
    VirtualDeviceBuilder::new()?
        .name(&nome)
        .with_keys(&teclas)?
        .with_relative_axes(&rel)?
        .build()
}

fn rel(eixo: RelativeAxisType, v: i32) -> InputEvent {
    InputEvent::new(EventType::RELATIVE, eixo.0, v)
}

fn botao_meio(v: i32) -> InputEvent {
    InputEvent::new(EventType::KEY, Key::BTN_MIDDLE.code(), v)
}

// Passo de rolagem a partir do deslocamento: fora da zona morta, linear.
fn passo(d: i32, vel: f64) -> f64 {
    let d = d as f64;
    let fora = (d.abs() - ZONA_MORTA).max(0.0);
    d.signum() * fora * vel * TICK.as_secs_f64()
}

fn rolar(estado: &Mutex<Estado>, saida: &Mutex<VirtualDevice>, vel: f64) {
    loop {
        thread::sleep(TICK);
        let mut eventos = Vec::new();
        {
            let mut e = estado.lock().unwrap();
            if !e.rolando {
                continue;
            }
            // mouse para baixo = conteúdo desce (roda negativa), igual ao Windows
            e.resto_v += -passo(e.dy, vel);
            e.resto_h += passo(e.dx, vel);
            let v = e.resto_v.trunc() as i32;
            let h = e.resto_h.trunc() as i32;
            e.resto_v -= v as f64;
            e.resto_h -= h as f64;
            if v != 0 {
                eventos.push(rel(RelativeAxisType::REL_WHEEL_HI_RES, v));
                e.notch_v += v;
                let n = e.notch_v / 120;
                if n != 0 {
                    e.notch_v -= n * 120;
                    eventos.push(rel(RelativeAxisType::REL_WHEEL, n));
                }
            }
            if h != 0 {
                eventos.push(rel(RelativeAxisType::REL_HWHEEL_HI_RES, h));
                e.notch_h += h;
                let n = e.notch_h / 120;
                if n != 0 {
                    e.notch_h -= n * 120;
                    eventos.push(rel(RelativeAxisType::REL_HWHEEL, n));
                }
            }
        }
        if !eventos.is_empty() {
            let _ = saida.lock().unwrap().emit(&eventos);
        }
    }
}

fn tratar(mut dev: Device, vel: f64) -> std::io::Result<()> {
    let saida = Arc::new(Mutex::new(criar_virtual(&dev)?));
    // dá tempo do compositor registrar o virtual antes de tirar o físico
    thread::sleep(Duration::from_millis(300));
    dev.grab()?;
    let estado = Arc::new(Mutex::new(Estado::default()));
    {
        let (estado, saida) = (estado.clone(), saida.clone());
        thread::spawn(move || rolar(&estado, &saida, vel));
    }
    loop {
        let lote: Vec<InputEvent> = dev.fetch_events()?.collect();
        let mut repassar = Vec::with_capacity(lote.len());
        {
            let mut e = estado.lock().unwrap();
            for ev in lote {
                match ev.event_type() {
                    EventType::SYNCHRONIZATION => continue,
                    EventType::KEY if ev.code() == Key::BTN_MIDDLE.code() => {
                        match ev.value() {
                            1 => {
                                *e = Estado { segurando: true, ..Estado::default() };
                            }
                            0 => {
                                if e.segurando && !e.rolando {
                                    repassar.push(botao_meio(1));
                                    saida.lock().unwrap().emit(&repassar)?;
                                    repassar.clear();
                                    repassar.push(botao_meio(0));
                                }
                                *e = Estado::default();
                            }
                            _ => {}
                        }
                        continue;
                    }
                    EventType::RELATIVE if e.segurando => {
                        if ev.code() == RelativeAxisType::REL_X.0 {
                            e.dx += ev.value();
                        } else if ev.code() == RelativeAxisType::REL_Y.0 {
                            e.dy += ev.value();
                        }
                        if !e.rolando && (e.dx.abs() > LIMIAR || e.dy.abs() > LIMIAR) {
                            e.rolando = true;
                        }
                    }
                    _ => {}
                }
                repassar.push(ev);
            }
        }
        if !repassar.is_empty() {
            saida.lock().unwrap().emit(&repassar)?;
        }
    }
}

fn main() {
    let vel = velocidade();
    let ativos: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
    loop {
        for (caminho, dev) in evdev::enumerate() {
            if !eh_mouse(&dev) || ativos.lock().unwrap().contains(&caminho) {
                continue;
            }
            eprintln!("autoscroll: {} ({})", dev.name().unwrap_or("?"), caminho.display());
            ativos.lock().unwrap().insert(caminho.clone());
            let ativos = ativos.clone();
            thread::spawn(move || {
                if let Err(err) = tratar(dev, vel) {
                    eprintln!("autoscroll: {} saiu: {err}", caminho.display());
                }
                ativos.lock().unwrap().remove(&caminho);
            });
        }
        thread::sleep(Duration::from_secs(2));
    }
}
