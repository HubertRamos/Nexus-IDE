//! Compartilhamento de servidores por workspace e linguagem.
//!
//! A unidade de compartilhamento não é a aba nem o documento: é o par
//! `(workspace, linguagem)`. Várias abas do mesmo projeto e da mesma linguagem
//! usam o mesmo [`LspServer`]; fechar uma aba envia apenas `didClose` para
//! aquele documento, mantendo o processo vivo para os demais.
//!
//! O pool é uma camada fina sobre [`crate::manager::LspServer`]. Ele não
//! conhece a TUI e não decide quando abrir arquivos; apenas garante que a
//! mesma chave nunca crie processos duplicados.

use std::collections::BTreeMap;

use crate::manager::{LspServer, ServerConfig, ServerError};

/// Identidade de um servidor compartilhado.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerKey {
    /// Raiz do workspace (caminho ou URI) usada para separar projetos.
    pub workspace: String,
    /// Identificador da linguagem, como `rust` ou `python`.
    pub language_id: String,
}

impl ServerKey {
    pub fn new(workspace: impl Into<String>, language_id: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            language_id: language_id.into(),
        }
    }
}

/// Conjunto de servidores indexados por workspace e linguagem.
#[derive(Debug, Default)]
pub struct ServerPool {
    servers: BTreeMap<ServerKey, LspServer>,
}

impl ServerPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.servers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn contains(&self, key: &ServerKey) -> bool {
        self.servers.contains_key(key)
    }

    pub fn get(&self, key: &ServerKey) -> Option<&LspServer> {
        self.servers.get(key)
    }

    pub fn get_mut(&mut self, key: &ServerKey) -> Option<&mut LspServer> {
        self.servers.get_mut(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &ServerKey> {
        self.servers.keys()
    }

    /// Remove um servidor do pool.
    ///
    /// Usado quando o processo encerrou ou o transporte ficou inconsistente.
    /// O `Drop` de [`LspServer`] mata o processo e aborta as tarefas, então a
    /// próxima abertura da mesma chave inicia um processo novo.
    pub fn remove(&mut self, key: &ServerKey) -> Option<LspServer> {
        self.servers.remove(key)
    }

    /// Retorna o servidor da chave, iniciando-o apenas na primeira vez.
    ///
    /// A configuração é usada somente quando o processo ainda não existe. Se
    /// outro chamador já iniciou o servidor, a configuração é ignorada para não
    /// substituir um processo compartilhado.
    pub async fn ensure(
        &mut self,
        key: ServerKey,
        config: ServerConfig,
    ) -> Result<&LspServer, ServerError> {
        if !self.servers.contains_key(&key) {
            let server = LspServer::start(config).await?;
            self.servers.insert(key.clone(), server);
        }

        Ok(self
            .servers
            .get(&key)
            .expect("servidor acabou de ser inserido"))
    }

    /// Encerra todos os servidores e devolve as falhas encontradas.
    pub async fn shutdown_all(&mut self) -> Vec<(ServerKey, ServerError)> {
        let keys: Vec<ServerKey> = self.servers.keys().cloned().collect();
        let mut errors = Vec::new();

        for key in keys {
            if let Some(mut server) = self.servers.remove(&key)
                && let Err(error) = server.shutdown().await
            {
                errors.push((key, error));
            }
        }

        errors
    }
}
