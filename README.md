# Otimizador Linux

App gráfico em Rust (egui) que analisa seu PC, sugere melhorias e recomenda a melhor distro.

## Rodar

```bash
cargo run --release
```

Dependências de build (Debian/Ubuntu):
```bash
sudo apt install build-essential pkg-config libxkbcommon-dev libwayland-dev libgl1-mesa-dev libx11-dev
```
Fedora: `sudo dnf install gcc pkgconf libxkbcommon-devel wayland-devel mesa-libGL-devel`
Arch: `sudo pacman -S base-devel libxkbcommon wayland mesa`

## O que ele faz

- **Resumo**: nota de 0 a 100 e ficha do hardware (CPU, RAM, discos, bateria, GPU, boot).
- **Sugestões**: dicas por categoria (bateria, CPU, disco, memória, sistema), com explicação e comando para copiar.
- **Processos**: quem mais consome CPU e memória.
- **Distro ideal**: ranking de distros conforme seu objetivo, seu nível e o hardware detectado.

O app **não altera nada sozinho**: só lê `/sys`, `/proc` e roda comandos de leitura (`lspci`, `systemctl is-enabled`, `systemd-analyze`, `iw`).
