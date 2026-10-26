// Autoscroll estilo Windows: segurar o botão do meio e arrastar rola a janela
// sob o cursor, com a curva de velocidade do Chromium/Edge. Reprodução em Rust
// do midscroll (github.com/gnhen/midscroll).
//
// Dois processos:
//   autoscroll daemon   root, serviço de sistema: grab nos mouses, espelho
//                       uinput, injeção da roda, socket de estado.
//   autoscroll overlay  usuário, serviço da sessão: desenha o selo e o
//                       cursor fantasma (wlr-layer-shell) e informa a janela
//                       em foco ao daemon (kdotool).

mod daemon;
mod overlay;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let resto = args.get(1..).unwrap_or(&[]).to_vec();
    match args.first().map(String::as_str) {
        Some("daemon") => daemon::main(resto),
        Some("overlay") => overlay::main(),
        _ => {
            eprintln!(
                "uso: autoscroll daemon [--config ARQ] [--debug] [--list-devices]\n       autoscroll overlay"
            );
            std::process::exit(2);
        }
    }
}
