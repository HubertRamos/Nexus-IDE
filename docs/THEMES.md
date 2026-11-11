# Temas da Nexus

A Nexus separa o tema da interface do tema de sintaxe. Os dois são resolvidos
de forma independente e podem ser combinados livremente, por exemplo interface
clara com sintaxe escura.

O crate `nexus-theme` não depende da TUI, de Ratatui ou de `nexus-core`. Ele
representa todas as cores em RGB e deixa a conversão para as capacidades do
terminal a cargo da interface. A futura GUI pode reutilizar o mesmo modelo.

## Temas internos

| Identificador          | Nome                  |
| ---------------------- | --------------------- |
| `nexus-dark`           | Nexus Dark            |
| `nexus-light`          | Nexus Light           |
| `nexus-high-contrast`  | Nexus High Contrast   |

`nexus-dark` é o padrão e também o fallback quando um tema solicitado não
existe, não define a seção necessária ou não pode ser carregado.

## Localização dos arquivos

Os caminhos seguem o diretório de configuração do usuário fornecido pela
biblioteca `dirs`:

- **Linux/BSD:** `$XDG_CONFIG_HOME/nexus` ou `~/.config/nexus`
- **Windows:** `%APPDATA%\Nexus`
- **macOS:** `~/Library/Application Support/nexus`

Dentro desse diretório:

```text
nexus/
├── config.toml        # seleção persistida de temas
└── themes/
    ├── meu-tema.toml
    └── outro-tema.toml
```

O diretório `themes/` é opcional. Arquivos com extensão diferente de `.toml`
são ignorados. O identificador do tema é o nome do arquivo sem extensão,
normalizado para minúsculas; um arquivo `Solarized.toml` cria o tema
`solarized`.

## Formato de um tema

```toml
schema_version = 1
name = "Meu Tema"
author = "Seu Nome"
version = "1.0.0"
description = "Tema pessoal da Nexus"

[ui]
background = "#101014"
surface = "#181820"
panel = "#20202a"
border = "#3a3a4a"
foreground = "#e6e6f0"
muted = "#8a8a9a"
accent = "#7aa2f7"
cursor = "#f5f5ff"
cursor_text = "#101014"
selection = "#33467c"
selection_text = "#ffffff"
current_line = "#1c1c26"
status_bar = "#181820"
status_bar_text = "#e6e6f0"
success = "#9ece6a"
error = "#f7768e"
warning = "#e0af68"
dialog = "#181820"

[syntax]
keyword = "#bb9af7"
string = "#9ece6a"
comment = "#565f89"
number = "#ff9e64"
function = "#7aa2f7"
method = "#7dcfff"
type = "#2ac3de"
variable = "#c0caf5"
constant = "#ff9e64"
operator = "#89ddff"
attribute = "#e0af68"
```

### Campos obrigatórios

- `schema_version`: atualmente deve ser `1`.
- `name`: nome legível, não vazio.
- Pelo menos uma das seções `[ui]` ou `[syntax]`.

Os campos `author`, `version` e `description` são opcionais.

### Cores

Todas as cores usam exatamente o formato `#RRGGBB`, com seis dígitos
hexadecimais. Formatos abreviados (`#fff`) e nomes de cores (`red`) são
rejeitados para evitar ambiguidade entre temas.

### Campos de interface

`background`, `surface`, `panel`, `border`, `foreground`, `muted`, `accent`,
`cursor`, `cursor_text`, `selection`, `selection_text`, `current_line`,
`status_bar`, `status_bar_text`, `success`, `error`, `warning`, `dialog`.

### Campos de sintaxe

`keyword`, `string`, `comment`, `number`, `function`, `method`, `type`,
`variable`, `constant`, `operator`, `attribute`.

O campo `type` é escrito assim no TOML, embora a estrutura Rust se chame
`type_name`.

### Validação

- Campos desconhecidos em qualquer seção são rejeitados.
- Um tema sem `[ui]` ou `[syntax]` é rejeitado.
- Um `schema_version` desconhecido é rejeitado.
- Um tema inválido é ignorado com aviso; os demais continuam disponíveis.
- Um tema externo não pode sobrescrever um tema interno com o mesmo
  identificador.
- Temas externos não podem incluir outros arquivos nem executar código.

Se um tema for válido mas não definir a seção solicitada (por exemplo, usado
como tema de interface quando só possui `[syntax]`), a seção ausente usa o
tema interno `nexus-dark` e um aviso é exibido na barra de status.

## Seleção persistida

A seleção fica em `config.toml`:

```toml
schema_version = 1

[theme]
ui = "nexus-light"
syntax = "nexus-high-contrast"
```

Ambos os campos são opcionais e usam `nexus-dark` quando ausentes. A
configuração é lida na inicialização. Esta etapa não inclui editor visual de
temas nem atalho para troca em tempo de execução: a seleção é feita editando o
arquivo e reiniciando a Nexus.

Configurações inválidas, com `schema_version` incompatível ou com temas
inexistentes não impedem a inicialização: a Nexus usa os temas padrão e mostra
um aviso.

## Capacidades de cor

A interface converte o RGB do tema para o terminal:

| Capacidade | Detecção                                              |
| ---------- | ----------------------------------------------------- |
| True Color | `NEXUS_COLOR=truecolor`, `COLORTERM=truecolor`/`24bit` |
| 256 cores  | `NEXUS_COLOR=256`, `TERM=*256color*`                  |
| 16 cores   | `NEXUS_COLOR=16`, fallback geral                      |
| Sem cor    | `NEXUS_COLOR=none`, `NO_COLOR`, `TERM=dumb`           |

`NEXUS_COLOR` tem precedência sobre as demais variáveis e aceita também
`24bit`, `ansi256`, `ansi` e `mono`.

No modo sem cor, seleção e cursor usam o modificador `REVERSED` do terminal em
vez de cores de fundo, preservando a distinção visual.

## Versionamento

`schema_version` permite evoluir o formato sem quebrar temas existentes. Temas
com versão diferente da suportada são ignorados com aviso. A configuração tem
seu próprio `schema_version`, independente do formato dos temas.

## Exemplo rápido

1. Crie `~/.config/nexus/themes/meu-tema.toml` (Linux) com o conteúdo acima.
2. Em `~/.config/nexus/config.toml`, selecione:

   ```toml
   schema_version = 1

   [theme]
   ui = "meu-tema"
   syntax = "nexus-dark"
   ```

3. Inicie a Nexus e abra um arquivo `.rs`, `.py`, `.js` ou `.ts`.

A ajuda interna (`Ctrl+H`) mostra os temas ativos, o modo de cor e o diretório
de temas em uso.
