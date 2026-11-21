//! Núcleo de integração LSP do Nexus.
//!
//! Este crate é deliberadamente independente de `nexus-tui` e de Ratatui. Ele
//! concentra o protocolo, o transporte, a conversão de posições UTF-16 e o
//! gerenciamento de servidores para que a camada de interface apenas consuma
//! uma API estável.
//!
//! A Etapa 1 implementa o envelope JSON-RPC 2.0, o enquadramento por
//! `Content-Length` e os tipos fundamentais de inicialização e sincronização.
//! A Etapa 2A acrescenta o gerenciamento do processo, o transporte STDIO e o
//! handshake `initialize`/`initialized`/`shutdown`/`exit`.
//!
//! # Organização
//!
//! - [`jsonrpc`]: mensagens JSON-RPC 2.0.
//! - [`transport`]: enquadramento `Content-Length` sobre STDIO.
//! - [`protocol`]: tipos e capacidades do protocolo LSP.
//! - [`utf16`]: conversão entre offsets do Nexus e posições UTF-16.
//! - [`uri`]: conversão entre caminhos locais e URIs `file://`.
//! - [`sync`]: sincronização de documentos conforme as capacidades do servidor.
//! - [`pool`]: compartilhamento de servidores por workspace e linguagem.
//! - [`service`]: fachada síncrona sobre o runtime assíncrono.
//! - [`manager`]: ciclo de vida de processos e sessões LSP.
//! - [`languages`]: presets de Rust e Python sobre a configuração genérica.

#![forbid(unsafe_code)]

pub mod jsonrpc;
pub mod languages;
pub mod manager;
pub mod pool;
pub mod protocol;
pub mod service;
pub mod sync;
pub mod transport;
pub mod uri;
pub mod utf16;
