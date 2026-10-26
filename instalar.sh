#!/bin/sh
# Rodar com pkexec: instala binário, config e serviço do sistema e (re)inicia.
# O serviço da sessão (overlay) é instalado por quem chamou, sem root.
set -e
cd "$(dirname "$0")"
install -m755 target/release/autoscroll /usr/local/bin/autoscroll
[ -e /etc/autoscroll.conf ] || install -m644 autoscroll.conf /etc/autoscroll.conf
install -m644 autoscroll.service /etc/systemd/system/autoscroll.service
systemctl daemon-reload
systemctl enable autoscroll.service
systemctl restart autoscroll.service
