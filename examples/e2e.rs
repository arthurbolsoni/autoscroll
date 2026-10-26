// Teste ponta a ponta: cria um mouse falso, o daemon pega ele; simula
// segurar o meio e arrastar 300 para baixo; lê o espelho e confere roda,
// cursor preso, salto do cursor ao soltar e clique simples.
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode, RelativeAxisCode as R};
use std::sync::mpsc;
use std::time::Duration;
use std::thread::sleep;

fn main() {
    let mut t = AttributeSet::<KeyCode>::new();
    for k in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE] { t.insert(k); }
    let mut r = AttributeSet::<R>::new();
    for a in [R::REL_X, R::REL_Y, R::REL_WHEEL] { r.insert(a); }
    let mut falso = VirtualDevice::builder().unwrap().name("autoscroll e2e")
        .input_id(evdev::InputId::new(evdev::BusType::BUS_USB, 0x1234, 0x5678, 1))
        .with_keys(&t).unwrap().with_relative_axes(&r).unwrap().build().unwrap();
    sleep(Duration::from_secs(5));
    let espelho = evdev::enumerate().find(|(_, d)| d.name() == Some("autoscroll e2e") && d.input_id().version() == 0xa5c0);
    let Some((p, mut esp)) = espelho else { println!("FALHA: daemon não criou espelho"); std::process::exit(1) };
    println!("espelho em {}", p.display());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        for e in esp.fetch_events().unwrap() { if e.event_type() != EventType::SYNCHRONIZATION { tx.send(e).unwrap(); } }
    });
    let ev = |t: EventType, c: u16, v: i32| InputEvent::new(t.0, c, v);
    // 1) arrasto
    falso.emit(&[ev(EventType::KEY, KeyCode::BTN_MIDDLE.0, 1)]).unwrap();
    for _ in 0..30 { falso.emit(&[ev(EventType::RELATIVE, R::REL_Y.0, 10)]).unwrap(); sleep(Duration::from_millis(5)); }
    sleep(Duration::from_millis(1000));
    falso.emit(&[ev(EventType::KEY, KeyCode::BTN_MIDDLE.0, 0)]).unwrap();
    sleep(Duration::from_millis(300));
    let evs: Vec<_> = rx.try_iter().collect();
    let soma = |c: R| evs.iter().filter(|e| e.event_type() == EventType::RELATIVE && e.code() == c.0).map(|e| e.value()).sum::<i32>();
    let meio = evs.iter().filter(|e| e.code() == KeyCode::BTN_MIDDLE.0 && e.event_type() == EventType::KEY).count();
    println!("arrasto: REL_Y={} WHEEL_HI_RES={} WHEEL={} cliques_meio={}", soma(R::REL_Y), soma(R::REL_WHEEL_HI_RES), soma(R::REL_WHEEL), meio);
    // 2) clique simples
    falso.emit(&[ev(EventType::KEY, KeyCode::BTN_MIDDLE.0, 1)]).unwrap();
    sleep(Duration::from_millis(60));
    falso.emit(&[ev(EventType::KEY, KeyCode::BTN_MIDDLE.0, 0)]).unwrap();
    sleep(Duration::from_millis(300));
    let evs: Vec<_> = rx.try_iter().collect();
    println!("clique: {:?}", evs.iter().map(|e| (e.code(), e.value())).collect::<Vec<_>>());
    // 3) movimento normal passa
    falso.emit(&[ev(EventType::RELATIVE, R::REL_X.0, 7)]).unwrap();
    sleep(Duration::from_millis(200));
    let evs: Vec<_> = rx.try_iter().collect();
    println!("movimento: {:?}", evs.iter().map(|e| (e.code(), e.value())).collect::<Vec<_>>());
    let _ = Device::open(&p);
}
