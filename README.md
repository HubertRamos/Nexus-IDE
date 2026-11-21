# Nexus IDE

**Nexus** é um editor de código terminal-first, desenvolvido em Rust, com foco em desempenho, controle pelo teclado, descoberta de comandos e uma experiência de edição acessível.

O projeto está sendo construído a partir de uma base modular: primeiro uma TUI funcional; posteriormente, uma interface gráfica poderá ampliar a experiência sem substituir o núcleo.

## Arquitetura

O repositório é um workspace Cargo com quatro crates:

| Crate | Responsabilidade |
| --- | --- |
| `nexus-core` | Núcleo de edição, buffer de texto e histórico de alterações. |
| `nexus-tui` | Interface de terminal e interação com o usuário. |
| `nexus-syntax` | Infraestrutura de análise sintática e highlighting. |
| `nexus-theme` | Temas e configuração visual. |

## Tecnologias

- **Rust** — linguagem principal.
- **Ropey** — estrutura de buffer de texto.
- **Ratatui** — renderização da interface de terminal.
- **Crossterm** — eventos e controle do terminal.
- **Tree-sitter** — parsing e suporte a syntax highlighting.

## Desenvolvimento

Requisitos: Rust e Cargo instalados.

Executar os testes do workspace:

```bash
cargo test --workspace
```

Executar a interface:

```bash
cargo run -p nexus-tui
```

## Direção do projeto

A Nexus busca combinar uma interface eficiente e orientada ao teclado com menus, dicas e suporte a mouse, evitando a curva de aprendizado desnecessariamente íngreme de editores tradicionais.

O projeto está em desenvolvimento. Recursos, suporte a linguagens e interfaces podem evoluir entre versões.

---

Desenvolvido por [Hubert Ramos](https://github.com/HubertRamos).
