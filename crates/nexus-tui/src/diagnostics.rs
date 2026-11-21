//! Projeções de diagnósticos para consumo pela TUI.
//!
//! O modelo recebido do LSP permanece sem dependência visual. Este módulo
//! resolve ranges para offsets do buffer e escolhe mensagens para a interface.

use nexus_lsp::protocol::DiagnosticSeverity;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::document_sync::{DiagnosticFreshness, DiagnosticsKey, StoredDiagnostics};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDiagnostic {
    pub start: usize,
    pub end: usize,
    pub severity: Option<DiagnosticSeverity>,
    pub freshness: DiagnosticFreshness,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiagnosticCounts {
    pub errors: usize,
    pub warnings: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProblemFilter {
    #[default]
    All,
    Errors,
    Warnings,
    Information,
    Hints,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProblemIdentity {
    pub key: DiagnosticsKey,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub severity: u8,
    pub source: Option<String>,
    pub code: Option<String>,
    pub duplicate: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProblemItem {
    pub identity: ProblemIdentity,
    pub path: PathBuf,
    pub file_label: String,
    pub line: usize,
    pub column: usize,
    pub severity: Option<DiagnosticSeverity>,
    pub freshness: DiagnosticFreshness,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProblemCounts {
    pub errors: usize,
    pub warnings: usize,
    pub information: usize,
    pub hints: usize,
}

type DuplicateProblemKey = (
    DiagnosticsKey,
    u32,
    u32,
    u32,
    u32,
    u8,
    Option<String>,
    Option<String>,
);

impl ProblemFilter {
    pub fn includes(self, severity: Option<DiagnosticSeverity>) -> bool {
        match self {
            Self::All => true,
            Self::Errors => severity == Some(DiagnosticSeverity::Error),
            Self::Warnings => severity == Some(DiagnosticSeverity::Warning),
            Self::Information => severity == Some(DiagnosticSeverity::Information),
            Self::Hints => severity == Some(DiagnosticSeverity::Hint),
        }
    }
}

/// Projeta o snapshot TUI para uma lista ordenada; o armazenamento permanece
/// em `App::diagnostics` e os itens nunca são mantidos em paralelo.
pub fn problem_items(diagnostics: &[StoredDiagnostics], filter: ProblemFilter) -> Vec<ProblemItem> {
    let mut items = Vec::new();
    let mut seen_snapshots = BTreeSet::new();
    let mut duplicates: BTreeMap<DuplicateProblemKey, usize> = BTreeMap::new();

    for stored in diagnostics
        .iter()
        .filter(|stored| stored.freshness != DiagnosticFreshness::PotentiallyStale)
    {
        if !seen_snapshots.insert(stored.key.clone()) {
            continue;
        }
        let Some(path) = nexus_lsp::uri::uri_to_path(&stored.key.uri) else {
            continue;
        };
        for diagnostic in &stored.publication.diagnostics {
            if (
                diagnostic.range.start.line,
                diagnostic.range.start.character,
            ) > (diagnostic.range.end.line, diagnostic.range.end.character)
            {
                continue;
            }
            if !filter.includes(diagnostic.severity) {
                continue;
            }
            let source = diagnostic.source.clone();
            let code = diagnostic.code.as_ref().map(code_label);
            let severity = severity_rank(diagnostic.severity);
            let duplicate_key = (
                stored.key.clone(),
                diagnostic.range.start.line,
                diagnostic.range.start.character,
                diagnostic.range.end.line,
                diagnostic.range.end.character,
                severity,
                source.clone(),
                code.clone(),
            );
            let duplicate = duplicates.entry(duplicate_key).or_default();
            let duplicate_index = *duplicate;
            *duplicate += 1;
            let identity = ProblemIdentity {
                key: stored.key.clone(),
                start_line: diagnostic.range.start.line,
                start_character: diagnostic.range.start.character,
                end_line: diagnostic.range.end.line,
                end_character: diagnostic.range.end.character,
                severity,
                source: source.clone(),
                code: code.clone(),
                duplicate: duplicate_index,
            };
            items.push(ProblemItem {
                identity,
                path: path.clone(),
                file_label: String::new(),
                line: diagnostic.range.start.line as usize + 1,
                column: diagnostic.range.start.character as usize + 1,
                severity: diagnostic.severity,
                freshness: stored.freshness,
                message: diagnostic.message.clone(),
                source,
                code,
            });
        }
    }

    label_problem_files(&mut items);
    items.sort_by(|left, right| {
        (
            severity_rank(left.severity),
            &left.identity.key.workspace,
            &left.path,
            left.identity.start_line,
            left.identity.start_character,
            &left.message,
            &left.identity,
        )
            .cmp(&(
                severity_rank(right.severity),
                &right.identity.key.workspace,
                &right.path,
                right.identity.start_line,
                right.identity.start_character,
                &right.message,
                &right.identity,
            ))
    });
    items
}

pub fn problem_counts(diagnostics: &[StoredDiagnostics]) -> ProblemCounts {
    let mut counts = ProblemCounts::default();
    let mut seen = BTreeSet::new();
    for stored in diagnostics
        .iter()
        .filter(|stored| stored.freshness != DiagnosticFreshness::PotentiallyStale)
    {
        if !seen.insert(stored.key.clone()) {
            continue;
        }
        for diagnostic in &stored.publication.diagnostics {
            if (
                diagnostic.range.start.line,
                diagnostic.range.start.character,
            ) > (diagnostic.range.end.line, diagnostic.range.end.character)
            {
                continue;
            }
            match diagnostic.severity {
                Some(DiagnosticSeverity::Error) => counts.errors += 1,
                Some(DiagnosticSeverity::Warning) => counts.warnings += 1,
                Some(DiagnosticSeverity::Information) => counts.information += 1,
                Some(DiagnosticSeverity::Hint) => counts.hints += 1,
                None => {}
            }
        }
    }
    counts
}

fn label_problem_files(items: &mut [ProblemItem]) {
    let mut base_counts = BTreeMap::<String, usize>::new();
    for item in items.iter() {
        *base_counts.entry(file_name(&item.path)).or_default() += 1;
    }
    for item in items.iter_mut() {
        let basename = file_name(&item.path);
        if base_counts.get(&basename).copied().unwrap_or_default() == 1 {
            item.file_label = basename;
            continue;
        }
        let root = Path::new(&item.identity.key.workspace);
        let relative = item.path.strip_prefix(root).unwrap_or(&item.path);
        let workspace_name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| item.identity.key.workspace.clone());
        item.file_label = format!("{workspace_name}/{}", relative.display());
    }
    let mut labels = BTreeMap::<String, usize>::new();
    for item in items.iter() {
        *labels.entry(item.file_label.clone()).or_default() += 1;
    }
    for item in items
        .iter_mut()
        .filter(|item| labels.get(&item.file_label).copied().unwrap_or_default() > 1)
    {
        let root = Path::new(&item.identity.key.workspace);
        let parent = root
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        item.file_label = format!("{parent}/{}", item.file_label);
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Converte somente diagnósticos utilizáveis para o texto e documento pedidos.
pub fn resolve_for_document(
    diagnostics: &[StoredDiagnostics],
    uri: &str,
    text: &str,
) -> Vec<ResolvedDiagnostic> {
    diagnostics
        .iter()
        .filter(|stored| {
            stored.key.uri == uri && stored.freshness != DiagnosticFreshness::PotentiallyStale
        })
        .flat_map(|stored| {
            stored
                .publication
                .diagnostics
                .iter()
                .filter_map(move |diagnostic| {
                    let start =
                        nexus_lsp::utf16::offset_from_position(text, diagnostic.range.start)
                            .ok()?;
                    let end =
                        nexus_lsp::utf16::offset_from_position(text, diagnostic.range.end).ok()?;
                    if start > end {
                        return None;
                    }
                    Some(ResolvedDiagnostic {
                        start,
                        end,
                        severity: diagnostic.severity,
                        freshness: stored.freshness,
                        message: diagnostic.message.clone(),
                        source: diagnostic.source.clone(),
                        code: diagnostic.code.as_ref().map(code_label),
                    })
                })
        })
        .collect()
}

/// Conta cada diagnóstico armazenado uma vez; snapshots por chave já substituem
/// versões anteriores. Resultados potencialmente obsoletos ficam de fora.
pub fn counts(diagnostics: &[StoredDiagnostics]) -> DiagnosticCounts {
    let mut counts = DiagnosticCounts::default();
    let mut seen = std::collections::BTreeSet::new();
    for stored in diagnostics
        .iter()
        .filter(|stored| stored.freshness != DiagnosticFreshness::PotentiallyStale)
    {
        if !seen.insert(&stored.key) {
            continue;
        }
        for diagnostic in &stored.publication.diagnostics {
            if (
                diagnostic.range.start.line,
                diagnostic.range.start.character,
            ) > (diagnostic.range.end.line, diagnostic.range.end.character)
            {
                continue;
            }
            match diagnostic.severity {
                Some(DiagnosticSeverity::Error) => counts.errors += 1,
                Some(DiagnosticSeverity::Warning) => counts.warnings += 1,
                _ => {}
            }
        }
    }
    counts
}

/// Seleciona mensagem sob o cursor por severidade e, em empate, intervalo,
/// fonte e mensagem para manter o resultado determinístico.
pub fn at_cursor(diagnostics: &[ResolvedDiagnostic], cursor: usize) -> Option<&ResolvedDiagnostic> {
    diagnostics
        .iter()
        .filter(|diagnostic| {
            if diagnostic.start == diagnostic.end {
                cursor == diagnostic.start
            } else {
                diagnostic.start <= cursor && cursor < diagnostic.end
            }
        })
        .min_by_key(|diagnostic| {
            (
                severity_rank(diagnostic.severity),
                diagnostic.end.saturating_sub(diagnostic.start),
                diagnostic.source.as_deref().unwrap_or(""),
                diagnostic.message.as_str(),
            )
        })
}

pub fn severity_rank(severity: Option<DiagnosticSeverity>) -> u8 {
    match severity {
        Some(DiagnosticSeverity::Error) => 0,
        Some(DiagnosticSeverity::Warning) => 1,
        Some(DiagnosticSeverity::Information) => 2,
        Some(DiagnosticSeverity::Hint) => 3,
        None => 4,
    }
}

fn code_label(code: &serde_json::Value) -> String {
    match code {
        serde_json::Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nexus_lsp::protocol::{Diagnostic, Position, Range};
    use serde_json::json;

    use super::*;
    use crate::document_sync::DiagnosticsKey;

    fn stored(
        uri: &str,
        workspace: &str,
        freshness: DiagnosticFreshness,
        diagnostics: Vec<Diagnostic>,
    ) -> StoredDiagnostics {
        StoredDiagnostics {
            key: DiagnosticsKey {
                workspace: workspace.into(),
                language_id: "rust".into(),
                session_id: 1,
                uri: uri.into(),
            },
            current_document_version: Some(1),
            publication: nexus_lsp::protocol::PublishDiagnosticsParams {
                uri: uri.into(),
                version: Some(1),
                diagnostics,
                extra: BTreeMap::new(),
            },
            freshness,
        }
    }

    fn diagnostic(
        start: Position,
        end: Position,
        severity: Option<DiagnosticSeverity>,
        message: &str,
    ) -> Diagnostic {
        Diagnostic {
            range: Range { start, end },
            severity,
            code: Some(json!("E7")),
            source: Some("check".into()),
            message: message.into(),
            code_description: None,
            tags: Vec::new(),
            related_information: Vec::new(),
            data: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn resolves_utf16_unicode_and_crlf_ranges_and_skips_invalid_positions() {
        let uri = "file:///a.rs";
        let text = "aé😀界\tz\r\nnext";
        let item = stored(
            uri,
            "/w",
            DiagnosticFreshness::Current,
            vec![
                diagnostic(
                    Position {
                        line: 0,
                        character: 2,
                    },
                    Position {
                        line: 0,
                        character: 5,
                    },
                    Some(DiagnosticSeverity::Error),
                    "emoji e ideograma",
                ),
                diagnostic(
                    Position {
                        line: 1,
                        character: 0,
                    },
                    Position {
                        line: 1,
                        character: 2,
                    },
                    Some(DiagnosticSeverity::Warning),
                    "crlf",
                ),
                diagnostic(
                    Position {
                        line: 9,
                        character: 0,
                    },
                    Position {
                        line: 9,
                        character: 1,
                    },
                    Some(DiagnosticSeverity::Hint),
                    "invalid",
                ),
            ],
        );
        let resolved = resolve_for_document(&[item], uri, text);
        assert_eq!((resolved[0].start, resolved[0].end), (2, 4));
        assert_eq!((resolved[1].start, resolved[1].end), (8, 10));
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn cursor_message_prioritizes_error_and_handles_empty_ranges() {
        let uri = "file:///a.rs";
        let item = stored(
            uri,
            "/w",
            DiagnosticFreshness::Unverified,
            vec![
                diagnostic(
                    Position {
                        line: 0,
                        character: 0,
                    },
                    Position {
                        line: 0,
                        character: 3,
                    },
                    Some(DiagnosticSeverity::Warning),
                    "warning",
                ),
                diagnostic(
                    Position {
                        line: 0,
                        character: 1,
                    },
                    Position {
                        line: 0,
                        character: 1,
                    },
                    Some(DiagnosticSeverity::Error),
                    "point error",
                ),
            ],
        );
        let resolved = resolve_for_document(&[item], uri, "abc");
        assert_eq!(at_cursor(&resolved, 1).unwrap().message, "point error");
        assert_eq!(at_cursor(&resolved, 2).unwrap().message, "warning");
        assert!(at_cursor(&resolved, 3).is_none());
    }

    #[test]
    fn stale_ranges_are_not_resolved_and_counts_aggregate_workspaces_once() {
        let a = stored(
            "file:///a.rs",
            "/one",
            DiagnosticFreshness::Current,
            vec![
                diagnostic(
                    Position {
                        line: 0,
                        character: 0,
                    },
                    Position {
                        line: 0,
                        character: 1,
                    },
                    Some(DiagnosticSeverity::Error),
                    "e",
                ),
                diagnostic(
                    Position {
                        line: 0,
                        character: 0,
                    },
                    Position {
                        line: 0,
                        character: 1,
                    },
                    Some(DiagnosticSeverity::Warning),
                    "w",
                ),
                diagnostic(
                    Position {
                        line: 0,
                        character: 0,
                    },
                    Position {
                        line: 0,
                        character: 1,
                    },
                    Some(DiagnosticSeverity::Information),
                    "i",
                ),
            ],
        );
        let b = stored(
            "file:///b.rs",
            "/two",
            DiagnosticFreshness::Unverified,
            vec![diagnostic(
                Position {
                    line: 0,
                    character: 0,
                },
                Position {
                    line: 0,
                    character: 1,
                },
                Some(DiagnosticSeverity::Error),
                "e2",
            )],
        );
        let stale = stored(
            "file:///c.rs",
            "/two",
            DiagnosticFreshness::PotentiallyStale,
            vec![diagnostic(
                Position {
                    line: 0,
                    character: 0,
                },
                Position {
                    line: 0,
                    character: 1,
                },
                Some(DiagnosticSeverity::Error),
                "stale",
            )],
        );
        assert_eq!(
            counts(&[a.clone(), b, stale.clone()]),
            DiagnosticCounts {
                errors: 2,
                warnings: 1
            }
        );
        assert!(resolve_for_document(&[stale], "file:///c.rs", "x").is_empty());
        assert_eq!(
            counts(&[a.clone(), a]),
            DiagnosticCounts {
                errors: 1,
                warnings: 1
            }
        );
    }

    #[test]
    fn problem_projection_orders_severity_filters_and_distinguishes_workspace_file_names() {
        let path_one = Path::new("/alpha/project/src/lib.rs");
        let path_two = Path::new("/beta/project/src/lib.rs");
        let uri_one = nexus_lsp::uri::path_to_uri(path_one);
        let uri_two = nexus_lsp::uri::path_to_uri(path_two);
        let one = stored(
            &uri_one,
            "/alpha/project",
            DiagnosticFreshness::Current,
            vec![
                diagnostic(
                    Position {
                        line: 1,
                        character: 2,
                    },
                    Position {
                        line: 1,
                        character: 3,
                    },
                    Some(DiagnosticSeverity::Warning),
                    "aviso",
                ),
                diagnostic(
                    Position {
                        line: 4,
                        character: 0,
                    },
                    Position {
                        line: 4,
                        character: 1,
                    },
                    Some(DiagnosticSeverity::Error),
                    "erro",
                ),
            ],
        );
        let two = stored(
            &uri_two,
            "/beta/project",
            DiagnosticFreshness::Unverified,
            vec![diagnostic(
                Position {
                    line: 0,
                    character: 0,
                },
                Position {
                    line: 0,
                    character: 1,
                },
                Some(DiagnosticSeverity::Information),
                "info",
            )],
        );
        let mut duplicate_snapshot = one.clone();
        duplicate_snapshot.freshness = DiagnosticFreshness::Unverified;
        let items = problem_items(&[one, two, duplicate_snapshot], ProblemFilter::All);
        assert_eq!(items.len(), 3, "snapshot repetido não duplica itens");
        assert_eq!(items[0].message, "erro", "erros vêm primeiro");
        assert_eq!(items[1].message, "aviso", "posição organiza itens no grupo");
        assert_eq!(items[2].message, "info");
        assert_ne!(
            items[1].file_label, items[2].file_label,
            "URI/workspace distinguem basenames iguais"
        );
        assert_eq!(
            problem_items(&items_to_storage_for_filter_test(), ProblemFilter::Errors).len(),
            1
        );
    }

    fn items_to_storage_for_filter_test() -> Vec<StoredDiagnostics> {
        let uri = nexus_lsp::uri::path_to_uri(Path::new("/workspace/a.rs"));
        vec![stored(
            &uri,
            "/workspace",
            DiagnosticFreshness::Current,
            vec![
                diagnostic(
                    Position {
                        line: 0,
                        character: 0,
                    },
                    Position {
                        line: 0,
                        character: 1,
                    },
                    Some(DiagnosticSeverity::Error),
                    "error",
                ),
                diagnostic(
                    Position {
                        line: 1,
                        character: 0,
                    },
                    Position {
                        line: 1,
                        character: 1,
                    },
                    Some(DiagnosticSeverity::Warning),
                    "warning",
                ),
            ],
        )]
    }
}
