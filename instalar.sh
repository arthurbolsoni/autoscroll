#!/bin/sh
# Rodar com pkexec: instala o binário e o serviço e (re)inicia.
set -e
cd "$(dirname "$0")"
install -m755 target/release/autoscroll /usr/local/bin/autoscroll
install -m644 autoscroll.service /etc/systemd/system/autoscroll.service
systemctl daemon-reload
systemctl enable autoscroll.service
systemctl restart autoscroll.service
