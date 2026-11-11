# Dependências da Nexus TUI 1.3

Este documento registra as dependências adicionadas para highlighting e temas,
com versão, licença e justificativa. A auditoria foi feita com
`cargo metadata --format-version 1`.

## Adicionadas nesta etapa

| Crate | Versão | Licença | Uso |
| ----- | ------ | ------- | --- |
| `tree-sitter` | 0.27.2 | MIT | Parser incremental e consultas de highlighting. |
| `tree-sitter-rust` | 0.24.2 | MIT | Gramática oficial de Rust. |
| `tree-sitter-python` | 0.25.0 | MIT | Gramática oficial de Python. |
| `tree-sitter-javascript` | 0.25.0 | MIT | Gramática oficial de JavaScript. |
| `tree-sitter-typescript` | 0.23.2 | MIT | Gramáticas oficiais de TypeScript e TSX. |
| `dirs` | 7.0.0 | MIT OR Apache-2.0 | Diretório de configuração do usuário em Linux, Windows e macOS. |
| `serde` | 1.0.229 | MIT OR Apache-2.0 | Derivação de `Deserialize`/`Serialize` para temas e configuração. |
| `toml` | 0.8.23 | MIT OR Apache-2.0 | Leitura e escrita dos arquivos TOML. |

As consultas em `crates/nexus-syntax/queries/` foram adaptadas das consultas
oficiais distribuídas com as gramáticas acima (licença MIT), removendo
predicados de texto que não são necessários no engine da Nexus.

## Dependências já existentes

| Crate | Licença | Uso |
| ----- | ------- | --- |
| `ropey` | MIT | Buffer de texto do Core. |
| `ratatui` | MIT | Renderização da TUI. |
| `crossterm` | MIT | Terminal, eventos e cores. |
| `unicode-width` | MIT OR Apache-2.0 | Largura visual de caracteres. |

## Dependências transitivas relevantes

A árvore completa usa licenças permissivas ou fracamente copyleft compatíveis
com a distribuição aberta da Nexus:

- `option-ext` 0.2.0: MPL-2.0 (dependência de `dirs`; licença em nível de
  arquivo, sem exigência de relicenciamento do projeto que a consome).
- `foldhash` 0.2.0: Zlib.
- `ryu` 1.0.23: Apache-2.0 OR BSL-1.0.
- `unicode-ident` 1.0.27: (MIT OR Apache-2.0) AND Unicode-3.0.
- Demais crates: MIT, Apache-2.0, MIT OR Apache-2.0, BSD ou equivalentes.

Nenhuma dependência GPL/AGPL foi introduzida. Nenhuma gramática exige
ferramentas externas em tempo de execução: as gramáticas são compiladas e
embutidas no binário.
