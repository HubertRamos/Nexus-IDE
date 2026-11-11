//! Documentos e gerenciamento de abas.
//!
//! Cada [`Document`] possui seu próprio Buffer, histórico, cursor, seleção,
//! rolagem, linguagem e estado de sintaxe. O [`DocumentManager`] mantém os
//! documentos que não estão ativos na ordem das abas; a aplicação troca o
//! documento ativo com um dos inativos ao alternar abas. Essa troca preserva
//! todo o estado sem clonar o Buffer (que não é `Clone`).

#![allow(dead_code)]

use std::fmt;
use std::path::{Path, PathBuf};

use nexus_core::buffer::Buffer;
use nexus_core::buffer::edit::BufferError;
use nexus_core::buffer::position::CharOffset;
use nexus_syntax::{LanguageId, SyntaxState};

use crate::files::{self, FileError};
use crate::view::View;

#[derive(Debug)]
pub enum DocumentError {
    Buffer(BufferError),
    File(FileError),
}

impl fmt::Display for DocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocumentError::Buffer(error) => write!(formatter, "{error}"),
            DocumentError::File(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for DocumentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DocumentError::Buffer(error) => Some(error),
            DocumentError::File(error) => Some(error),
        }
    }
}

impl From<BufferError> for DocumentError {
    fn from(error: BufferError) -> Self {
        DocumentError::Buffer(error)
    }
}

impl From<FileError> for DocumentError {
    fn from(error: FileError) -> Self {
        DocumentError::File(error)
    }
}

/// Estado completo de um documento aberto.
#[derive(Debug)]
pub struct Document {
    pub buffer: Buffer,
    pub view: View,
    pub path: Option<PathBuf>,
    pub name: String,
    pub line_ending: String,
    pub syntax: Option<SyntaxState>,
    /// Conteúdo/versão conhecidos do disco no último carregamento ou gravação.
    pub disk: Option<crate::watcher::FileFingerprint>,
    /// Documento restaurado de uma cópia de recuperação.
    pub recovered: bool,
    /// O arquivo associado desapareceu do disco.
    pub missing: bool,
    /// O arquivo mudou no disco enquanto havia edições locais.
    pub conflict: bool,
    /// Identificador estável usado pelo Recovery Manager.
    pub recovery_id: Option<String>,
}

impl Document {
    pub fn empty() -> Self {
        Self::from_text("sem título", "")
    }

    pub fn from_text(name: impl Into<String>, text: &str) -> Self {
        Self {
            buffer: Buffer::from_text(text),
            view: View::new(),
            path: None,
            name: name.into(),
            line_ending: detect_line_ending(text).to_owned(),
            syntax: None,
            disk: None,
            recovered: false,
            missing: false,
            conflict: false,
            recovery_id: None,
        }
    }

    pub fn from_unsaved_text(name: impl Into<String>, path: Option<PathBuf>, text: &str) -> Self {
        let syntax = path
            .as_deref()
            .and_then(LanguageId::from_path)
            .map(|language| SyntaxState::new(language, text));

        let mut buffer = Buffer::from_text(text);
        let _ = buffer.mark_unsaved();

        Self {
            buffer,
            view: View::new(),
            name: name.into(),
            line_ending: detect_line_ending(text).to_owned(),
            path,
            syntax,
            disk: None,
            recovered: true,
            missing: false,
            conflict: false,
            recovery_id: None,
        }
    }

    pub fn from_file(path: &Path) -> Result<Self, DocumentError> {
        let text = files::read_document(path)?;
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let disk = crate::watcher::fingerprint(&canonical).ok().flatten();
        let syntax =
            LanguageId::from_path(&canonical).map(|language| SyntaxState::new(language, &text));

        Ok(Self {
            buffer: Buffer::from_text(&text),
            view: View::new(),
            name: document_name(&canonical),
            path: Some(canonical),
            line_ending: detect_line_ending(&text).to_owned(),
            syntax,
            disk,
            recovered: false,
            missing: false,
            conflict: false,
            recovery_id: None,
        })
    }

    pub fn is_modified(&self) -> bool {
        self.buffer.is_modified()
    }

    pub fn cursor(&self) -> CharOffset {
        self.view.cursor()
    }

    pub fn selection(&self) -> Option<nexus_core::buffer::edit::TextRange> {
        self.view.selection_range()
    }

    pub fn top_line(&self) -> usize {
        self.view.top_line()
    }

    pub fn left_col(&self) -> usize {
        self.view.left_col()
    }

    /// Atualiza caminho, nome e linguagem sem tocar no conteúdo.
    pub fn set_path(&mut self, path: &Path, text: &str) {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.name = document_name(&canonical);
        self.path = Some(canonical);
        self.set_syntax_for_path(path, text);
    }

    /// Recalcula a linguagem sem recriar a árvore quando ela já corresponde.
    pub fn set_syntax_for_path(&mut self, path: &Path, text: &str) {
        let language = LanguageId::from_path(path);

        match (&self.syntax, language) {
            (Some(state), Some(language)) if state.language() == language => {}
            (None, None) => {}
            _ => {
                self.syntax = language.map(|language| SyntaxState::new(language, text));
            }
        }
    }

    /// Substitui o conteúdo por uma versão recarregada do disco, preservando
    /// cursor e rolagem dentro dos limites do novo conteúdo.
    pub fn reload_text(&mut self, text: &str) {
        let cursor = self.view.cursor();
        let anchor = self.view.selection_range();
        self.buffer = Buffer::from_text(text);
        self.line_ending = detect_line_ending(text).to_owned();
        self.recovered = false;
        self.missing = false;
        self.conflict = false;
        self.syntax = self
            .path
            .as_deref()
            .and_then(LanguageId::from_path)
            .map(|language| SyntaxState::new(language, text));

        let _ = self.view.set_cursor(&self.buffer, cursor);
        if let Some(range) = anchor {
            let _ = self
                .view
                .set_selection(&self.buffer, range.start, range.end);
        }
    }

    pub fn mark_disk_current(&mut self) {
        if let Some(path) = &self.path {
            self.disk = crate::watcher::fingerprint(path).ok().flatten();
        }
    }

    /// Instala conteúdo recuperado como uma versão não salva.
    pub fn install_recovered(&mut self, text: &str) {
        self.buffer = Buffer::from_text(text);
        let _ = self.buffer.mark_unsaved();
        self.line_ending = detect_line_ending(text).to_owned();
        self.recovered = true;
        self.missing = false;
        self.conflict = false;
        self.syntax = self
            .path
            .as_deref()
            .and_then(LanguageId::from_path)
            .map(|language| SyntaxState::new(language, text));
    }

    /// Restaura cursor, seleção e rolagem sem alterar o conteúdo.
    pub fn restore_view_state(
        &mut self,
        cursor: CharOffset,
        anchor: Option<CharOffset>,
        top_line: usize,
        left_col: usize,
    ) {
        let _ = self
            .view
            .restore_state(&self.buffer, cursor, anchor, top_line, left_col);
    }
}

/// Informações resumidas de uma aba, calculadas a partir do estado atual.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    pub index: usize,
    pub name: String,
    pub path: Option<PathBuf>,
    pub modified: bool,
    pub active: bool,
}

/// Mantém os documentos inativos na ordem conceitual das abas.
///
/// O documento ativo fica fora do vetor para permitir edição exclusiva. Os
/// índices usados pela interface são índices conceituais, incluindo o ativo.
#[derive(Debug, Default)]
pub struct DocumentManager {
    inactive: Vec<Document>,
    active_index: usize,
}

impl DocumentManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.inactive.len() + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn active_index(&self) -> usize {
        self.active_index
    }

    /// Define o índice ativo depois de reconstruir uma sessão.
    pub fn set_active_index(&mut self, index: usize) {
        self.active_index = index.min(self.inactive.len());
    }

    pub fn inactive(&self) -> &[Document] {
        &self.inactive
    }

    pub fn inactive_mut(&mut self) -> &mut [Document] {
        &mut self.inactive
    }

    /// Adiciona um documento inativo ao fim da ordem das abas.
    pub fn push_inactive(&mut self, document: Document) {
        self.inactive.push(document);
    }

    /// Índice conceitual de um caminho, comparando caminhos canônicos.
    pub fn index_of_path(&self, active: &Document, path: &Path) -> Option<usize> {
        let target = canonical_or_owned(path);

        for (index, document) in self.tabs(active).into_iter().enumerate() {
            if let Some(candidate) = document.path.as_deref()
                && canonical_or_owned(candidate) == target
            {
                return Some(index);
            }
        }

        None
    }

    pub fn contains_path(&self, active: &Document, path: &Path) -> bool {
        self.index_of_path(active, path).is_some()
    }

    pub fn tabs<'a>(&'a self, active: &'a Document) -> Vec<&'a Document> {
        let mut documents: Vec<&Document> = self.inactive.iter().collect();
        let position = self.active_index.min(documents.len());
        documents.insert(position, active);
        documents
    }

    pub fn tab_infos(&self, active: &Document) -> Vec<TabInfo> {
        self.tabs(active)
            .into_iter()
            .enumerate()
            .map(|(index, document)| TabInfo {
                index,
                name: document.name.clone(),
                path: document.path.clone(),
                modified: document.is_modified() || document.recovered,
                active: index == self.active_index,
            })
            .collect()
    }

    /// Insere um novo documento logo após o ativo e o torna ativo.
    pub fn add_after_active(&mut self, active: &mut Document, mut new_document: Document) {
        let content_size = active.view.content_size();
        new_document
            .view
            .set_content_size(content_size.0, content_size.1);
        self.inactive
            .insert(self.active_index, std::mem::replace(active, new_document));
        self.active_index += 1;
    }

    /// Ativa o documento no índice conceitual, preservando o estado de ambos.
    pub fn switch_to(&mut self, active: &mut Document, target: usize) -> bool {
        if target >= self.len() || target == self.active_index {
            return false;
        }

        let old_index = self.active_index;
        let mut target_document = if target < old_index {
            self.inactive.remove(target)
        } else {
            self.inactive.remove(target - 1)
        };

        let content_size = active.view.content_size();
        std::mem::swap(active, &mut target_document);
        target_document
            .view
            .set_content_size(content_size.0, content_size.1);

        let insert_at = if target < old_index {
            target
        } else {
            old_index
        };
        self.inactive.insert(insert_at, target_document);
        self.active_index = target;
        true
    }

    /// Remove um documento inativo. O índice precisa ser conceitual e não pode
    /// apontar para o ativo.
    pub fn close_inactive(&mut self, index: usize) -> bool {
        if index == self.active_index || index >= self.len() {
            return false;
        }

        if index < self.active_index {
            self.inactive.remove(index);
            self.active_index -= 1;
        } else {
            self.inactive.remove(index - 1);
        }

        true
    }

    /// Remove o documento ativo e ativa o vizinho mais próximo.
    ///
    /// Retorna `false` quando não havia outro documento; nesse caso o chamador
    /// deve criar um documento vazio.
    pub fn close_active(&mut self, active: &mut Document) -> bool {
        if self.inactive.is_empty() {
            *active = Document::empty();
            self.active_index = 0;
            return false;
        }

        let target = self.active_index.min(self.inactive.len() - 1);
        let mut replacement = self.inactive.remove(target);
        let content_size = active.view.content_size();
        std::mem::swap(active, &mut replacement);
        replacement
            .view
            .set_content_size(content_size.0, content_size.1);
        self.active_index = target;
        true
    }

    /// Remove todos os documentos cujo caminho esteja sob `path`.
    ///
    /// `path` pode ser um arquivo ou um diretório. Um documento é afetado
    /// quando seu caminho canônico é igual a `path` ou está contido nele. O
    /// documento ativo é substituído por um vazio quando também é afetado.
    /// Retorna os índices conceituais removidos, em ordem crescente.
    pub fn remove_paths_under(&mut self, active: &mut Document, path: &Path) -> Vec<usize> {
        let target = canonical_or_owned(path);
        let mut removed = Vec::new();

        // O documento ativo ocupa o índice conceitual `active_index`; os
        // inativos ocupam os demais, deslocados em uma posição a partir dele.
        let active_affected = active
            .path
            .as_deref()
            .is_some_and(|candidate| path_is_under(candidate, &target));

        // Coleta primeiro os índices físicos dos inativos afetados e os seus
        // índices conceituais. Avançar sempre o índice evita laços infinitos e
        // permite remover em uma segunda passada, de trás para frente.
        let mut inactive_to_remove = Vec::new();
        for (index, document) in self.inactive.iter().enumerate() {
            if document
                .path
                .as_deref()
                .is_some_and(|candidate| path_is_under(candidate, &target))
            {
                let conceptual = if index < self.active_index {
                    index
                } else {
                    index + 1
                };
                inactive_to_remove.push(index);
                removed.push(conceptual);
            }
        }

        if active_affected {
            removed.push(self.active_index);
        }
        removed.sort_unstable();

        // Remover do maior índice físico para o menor mantém válidos os
        // índices restantes. Cada remoção antes do ativo desloca o ativo.
        for index in inactive_to_remove.into_iter().rev() {
            self.inactive.remove(index);
            if index < self.active_index {
                self.active_index -= 1;
            }
        }

        if active_affected {
            // O documento ativo foi removido: ele vira um documento vazio na
            // mesma posição conceitual. Índices inativos posteriores ao ativo
            // permanecem depois dele.
            *active = Document::empty();
        }

        // Após remover todos os afetados, o índice ativo precisa continuar
        // válido mesmo que os inativos posteriores tenham sido removidos.
        self.active_index = self.active_index.min(self.inactive.len());

        removed
    }

    /// Substitui o documento ativo por um vazio e descarta os demais.
    pub fn reset(&mut self, active: &mut Document) {
        self.inactive.clear();
        self.active_index = 0;
        *active = Document::empty();
    }
}

fn path_is_under(candidate: &Path, target: &Path) -> bool {
    // `Path::starts_with` compara componentes, não texto: `/a/bc` não está
    // sob `/a/b`. Caminhos iguais também contam como "sob" (remoção de um
    // arquivo aberto).
    canonical_or_owned(candidate).starts_with(target)
}

/// Resolve um caminho para comparação, mesmo quando ele não existe mais.
///
/// `canonicalize` falha para caminhos removidos (o caso típico ao excluir
/// arquivos do disco), então caímos para uma normalização lexical que resolve
/// `.`/`..` e garante que caminhos relativos sejam comparados em relação ao
/// diretório atual.
fn canonical_or_owned(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    // O próprio caminho pode ter sido removido (exclusão de arquivo), mas o
    // diretório pai normalmente continua existindo. Canonicalizar o pai
    // preserva a equivalência com caminhos já resolvidos, inclusive quando o
    // diretório temporário é um link simbólico.
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(canonical_parent) = std::fs::canonicalize(parent)
    {
        return canonical_parent.join(name);
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    normalize_lexically(&absolute)
}

/// Remove componentes `.` e resolve `..` sem tocar no sistema de arquivos.
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

pub fn document_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

pub fn detect_line_ending(text: &str) -> &'static str {
    match text.find('\n') {
        Some(index) if index > 0 && text.as_bytes()[index - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nexus-document-{}-{counter}-{label}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn active(text: &str) -> Document {
        Document::from_text("sem título", text)
    }

    #[test]
    fn opening_same_path_twice_is_detected() {
        let directory = TempDir::new("duplicate");
        let path = directory.join("a.txt");
        fs::write(&path, "a").unwrap();
        let mut manager = DocumentManager::new();
        let mut current = Document::from_file(&path).unwrap();
        let second = Document::from_file(&path).unwrap();

        manager.add_after_active(&mut current, second);
        assert_eq!(manager.len(), 2);
        assert!(manager.contains_path(&current, &path));
    }

    #[test]
    fn switching_tabs_preserves_buffer_cursor_and_selection() {
        let mut manager = DocumentManager::new();
        let mut current = Document::from_text("primeiro", "abc");
        let mut second = Document::from_text("segundo", "xyz");
        second.view.set_content_size(20, 5);
        second
            .view
            .set_cursor(&second.buffer, CharOffset(2))
            .unwrap();
        second
            .view
            .set_selection(&second.buffer, CharOffset(1), CharOffset(2))
            .unwrap();

        manager.add_after_active(&mut current, second);
        assert_eq!(current.name, "segundo");
        assert_eq!(current.view.cursor(), CharOffset(2));

        assert!(manager.switch_to(&mut current, 0));
        assert_eq!(current.name, "primeiro");
        assert_eq!(current.buffer.text().unwrap(), "abc");

        assert!(manager.switch_to(&mut current, 1));
        assert_eq!(current.name, "segundo");
        assert_eq!(current.buffer.text().unwrap(), "xyz");
        assert_eq!(current.view.cursor(), CharOffset(2));
        assert_eq!(
            current.view.selection_range().map(|range| range.start),
            Some(CharOffset(1))
        );
    }

    #[test]
    fn closing_active_selects_neighbor() {
        let mut manager = DocumentManager::new();
        let mut current = Document::from_text("a", "a");
        manager.add_after_active(&mut current, Document::from_text("b", "b"));
        manager.add_after_active(&mut current, Document::from_text("c", "c"));
        assert_eq!(manager.len(), 3);

        assert!(manager.close_active(&mut current));
        assert_eq!(manager.len(), 2);
        assert_eq!(current.name, "b");
    }

    #[test]
    fn closing_last_document_creates_empty_document() {
        let mut manager = DocumentManager::new();
        let mut current = Document::from_text("a", "a");

        assert!(!manager.close_active(&mut current));
        assert_eq!(manager.len(), 1);
        assert_eq!(current.name, "sem título");
        assert_eq!(current.buffer.text().unwrap(), "");
    }

    #[test]
    fn removing_paths_under_directory_updates_tabs() {
        let directory = TempDir::new("remove-under");
        let nested = directory.join("src");
        fs::create_dir_all(&nested).unwrap();
        let one = nested.join("a.rs");
        let two = nested.join("b.rs");
        fs::write(&one, "a").unwrap();
        fs::write(&two, "b").unwrap();

        let mut manager = DocumentManager::new();
        let mut current = Document::from_file(&one).unwrap();
        manager.add_after_active(&mut current, Document::from_file(&two).unwrap());

        let removed = manager.remove_paths_under(&mut current, &directory.path);
        assert_eq!(removed.len(), 2);
        assert_eq!(manager.len(), 1);
        assert_eq!(current.name, "sem título");
    }

    #[test]
    fn removes_single_file_and_keeps_other_tabs() {
        let directory = TempDir::new("remove-single");
        let one = directory.join("a.txt");
        let two = directory.join("b.txt");
        fs::write(&one, "a").unwrap();
        fs::write(&two, "b").unwrap();

        let mut manager = DocumentManager::new();
        let mut current = Document::from_file(&one).unwrap();
        manager.add_after_active(&mut current, Document::from_file(&two).unwrap());

        let removed = manager.remove_paths_under(&mut current, &two);

        assert_eq!(removed, vec![1]);
        assert_eq!(manager.len(), 2);
        assert!(current.path.is_none());
        assert_eq!(current.name, "sem título");
        let infos = manager.tab_infos(&current);
        assert_eq!(infos[0].name, "a.txt");
        assert_eq!(infos[1].name, "sem título");
        assert!(infos[1].active);
    }

    #[test]
    fn removes_multiple_files_in_one_pass() {
        let directory = TempDir::new("remove-multiple");
        let one = directory.join("a.rs");
        let two = directory.join("b.rs");
        let three = directory.join("c.rs");
        for path in [&one, &two, &three] {
            fs::write(path, "x").unwrap();
        }

        let mut manager = DocumentManager::new();
        let mut current = Document::from_file(&one).unwrap();
        manager.add_after_active(&mut current, Document::from_file(&two).unwrap());
        manager.add_after_active(&mut current, Document::from_file(&three).unwrap());

        // Remove os arquivos `a` e `b`, mantendo `c` ativo.
        let removed_a = manager.remove_paths_under(&mut current, &one);
        let removed_b = manager.remove_paths_under(&mut current, &two);

        assert_eq!(removed_a, vec![0]);
        assert_eq!(removed_b, vec![0]);
        assert_eq!(manager.len(), 1);
        assert_eq!(current.name, "c.rs");
        assert_eq!(current.path.as_deref(), Some(three.as_path()));
        assert!(manager.tab_infos(&current)[0].active);
    }

    #[test]
    fn removing_paths_under_keeps_active_index_valid() {
        let directory = TempDir::new("remove-index");
        let one = directory.join("a.txt");
        let two = directory.join("b.txt");
        let three = directory.join("c.txt");
        for path in [&one, &two, &three] {
            fs::write(path, "x").unwrap();
        }

        let mut manager = DocumentManager::new();
        let mut current = Document::from_file(&one).unwrap();
        manager.add_after_active(&mut current, Document::from_file(&two).unwrap());
        manager.add_after_active(&mut current, Document::from_file(&three).unwrap());
        assert_eq!(manager.active_index(), 2);

        let removed = manager.remove_paths_under(&mut current, &one);

        assert_eq!(removed, vec![0]);
        assert_eq!(manager.len(), 2);
        assert_eq!(manager.active_index(), 1);
        let infos = manager.tab_infos(&current);
        assert_eq!(infos[0].name, "b.txt");
        assert_eq!(infos[1].name, "c.txt");
        assert!(infos[1].active);
        assert_eq!(current.name, "c.txt");
    }

    #[test]
    fn removes_nonexistent_file_using_equivalent_paths() {
        let directory = TempDir::new("remove-equivalent");
        let nested = directory.join("src");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("a.rs");
        fs::write(&file, "a").unwrap();

        let mut manager = DocumentManager::new();
        let mut current = Document::from_file(&file).unwrap();

        // O arquivo já foi removido do disco; a comparação precisa funcionar
        // mesmo quando `canonicalize` falha.
        fs::remove_file(&file).unwrap();
        let equivalent = nested.join(".").join("a.rs");
        let removed = manager.remove_paths_under(&mut current, &file);

        assert_eq!(removed, vec![0]);
        assert_eq!(manager.len(), 1);
        assert_eq!(current.name, "sem título");

        // Mesmo com o documento já removido, caminhos equivalentes não
        // produzem correspondências espúrias.
        assert!(
            manager
                .remove_paths_under(&mut current, &equivalent)
                .is_empty()
        );
    }

    #[test]
    fn removing_unrelated_path_keeps_documents_intact() {
        let directory = TempDir::new("remove-none");
        let nested = directory.join("src");
        fs::create_dir_all(&nested).unwrap();
        let one = nested.join("a.rs");
        let two = nested.join("b.rs");
        fs::write(&one, "a").unwrap();
        fs::write(&two, "b").unwrap();

        let mut manager = DocumentManager::new();
        let mut current = Document::from_file(&one).unwrap();
        manager.add_after_active(&mut current, Document::from_file(&two).unwrap());
        let before = manager.tab_infos(&current);

        let unrelated = directory.join("outros");
        let removed = manager.remove_paths_under(&mut current, &unrelated);

        assert!(removed.is_empty());
        assert_eq!(manager.len(), 2);
        assert_eq!(manager.active_index(), 1);
        assert_eq!(manager.tab_infos(&current), before);
    }

    #[test]
    fn removing_directory_does_not_match_similar_prefix() {
        let directory = TempDir::new("remove-prefix");
        let src = directory.join("src");
        let src_extra = directory.join("src-extra");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&src_extra).unwrap();
        let inside = src.join("a.rs");
        let similar = src_extra.join("b.rs");
        fs::write(&inside, "a").unwrap();
        fs::write(&similar, "b").unwrap();

        let mut manager = DocumentManager::new();
        let mut current = Document::from_file(&inside).unwrap();
        manager.add_after_active(&mut current, Document::from_file(&similar).unwrap());

        let removed = manager.remove_paths_under(&mut current, &src);

        assert_eq!(removed, vec![0]);
        assert_eq!(manager.len(), 1);
        assert_eq!(manager.active_index(), 0);
        assert_eq!(current.name, "b.rs");
        assert_eq!(current.path.as_deref(), Some(similar.as_path()));
        assert_eq!(manager.tab_infos(&current)[0].name, "b.rs");
    }

    #[test]
    fn tab_infos_marks_active_and_modified() {
        let mut manager = DocumentManager::new();
        let mut current = Document::from_text("a", "a");
        manager.add_after_active(&mut current, Document::from_text("b", "b"));
        let infos = manager.tab_infos(&current);

        assert_eq!(infos.len(), 2);
        assert!(infos[1].active);
        assert!(!infos[0].active);
    }

    #[test]
    fn unsaved_document_starts_modified() {
        let document = Document::from_unsaved_text("recuperado", None, "conteúdo");

        assert!(document.is_modified());
        assert!(document.recovered);
    }

    #[test]
    fn detects_crlf() {
        assert_eq!(detect_line_ending("a\r\nb"), "\r\n");
        assert_eq!(detect_line_ending("a\nb"), "\n");
    }
}
