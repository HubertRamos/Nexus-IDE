//! Conversão entre caminhos locais e URIs `file://` usadas pelo LSP.
//!
//! O LSP identifica documentos por URI, não por caminho. A conversão precisa
//! preservar a distinção entre arquivos com o mesmo nome em diretórios
//! diferentes e codificar caracteres especiais (espaços, acentos e símbolos)
//! como bytes UTF-8 percent-encoded.

use std::path::{Path, PathBuf};

/// Converte um caminho local em uma URI `file://` absoluta.
pub fn path_to_uri(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    let text = absolute.to_string_lossy().replace('\\', "/");
    let text = strip_verbatim_prefix(text);

    #[cfg(windows)]
    {
        if let Some(rest) = text.strip_prefix("//") {
            // UNC: //servidor/compartilhamento -> file://servidor/compartilhamento
            return format!("file://{}", percent_encode(rest));
        }

        let text = if text.starts_with('/') {
            text
        } else {
            format!("/{text}")
        };
        format!("file://{}", percent_encode(&text))
    }

    #[cfg(not(windows))]
    {
        let text = if text.starts_with('/') {
            text
        } else {
            format!("/{text}")
        };
        format!("file://{}", percent_encode(&text))
    }
}

/// Converte uma URI `file://` em um caminho local.
///
/// URIs com autoridade remota não são aceitas em plataformas que não suportam
/// caminhos UNC; nesses casos a função retorna `None`.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;

    #[cfg(windows)]
    {
        if !rest.starts_with('/') {
            // UNC: file://servidor/compartilhamento
            let (authority, path) = rest.split_once('/')?;
            let decoded = percent_decode(path)?;
            let decoded = String::from_utf8(decoded).ok()?;
            return Some(PathBuf::from(format!(
                "//{authority}/{}",
                decoded.trim_start_matches('/')
            )));
        }

        let path = rest;
        let decoded = percent_decode(path)?;
        let decoded = String::from_utf8(decoded).ok()?;
        let decoded = decoded.strip_prefix('/').unwrap_or(&decoded);
        return Some(PathBuf::from(decoded.replace('/', "\\")));
    }

    #[cfg(not(windows))]
    {
        let path = if rest.starts_with('/') {
            rest
        } else {
            rest.strip_prefix("localhost")?
        };

        let decoded = percent_decode(path)?;
        let decoded = String::from_utf8(decoded).ok()?;
        Some(PathBuf::from(decoded))
    }
}

/// Remove o prefixo verbatim que `canonicalize` produz no Windows.
///
/// `\\?\C:\projeto` vira `C:/projeto` e `\\?\UNC\servidor\share` vira
/// `//servidor/share`. Sem isso, a URI trataria `?` como autoridade UNC.
fn strip_verbatim_prefix(text: String) -> String {
    if let Some(rest) = text.strip_prefix("//?/UNC/") {
        return format!("//{rest}");
    }
    if let Some(rest) = text.strip_prefix("//?/") {
        return rest.to_owned();
    }
    text
}

fn percent_encode(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len());

    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }

    encoded
}

fn percent_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    Some(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_paths_are_encoded_and_decoded() {
        let path = Path::new("/tmp/projetos/ação com espaço/main.rs");
        let uri = path_to_uri(path);

        assert_eq!(
            uri,
            "file:///tmp/projetos/a%C3%A7%C3%A3o%20com%20espa%C3%A7o/main.rs"
        );
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn emoji_and_reserved_characters_are_encoded() {
        let path = Path::new("/tmp/😀#?%arquivo.rs");
        let uri = path_to_uri(path);

        assert!(uri.contains("%F0%9F%98%80"));
        assert!(uri.contains("%23"));
        assert!(uri.contains("%3F"));
        assert!(uri.contains("%25"));
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn same_name_in_different_directories_has_different_uri() {
        let first = path_to_uri(Path::new("/tmp/a/main.rs"));
        let second = path_to_uri(Path::new("/tmp/b/main.rs"));

        assert_ne!(first, second);
        assert!(first.ends_with("/a/main.rs"));
        assert!(second.ends_with("/b/main.rs"));
    }

    #[test]
    fn relative_paths_become_absolute() {
        let uri = path_to_uri(Path::new("src/main.rs"));
        let path = uri_to_path(&uri).expect("URI deveria ser convertida");

        assert!(path.is_absolute());
        assert!(path.ends_with("src/main.rs"));
    }

    #[test]
    fn windows_verbatim_prefixes_are_removed_before_uri_conversion() {
        assert_eq!(
            strip_verbatim_prefix("//?/C:/projetos/main.rs".to_owned()),
            "C:/projetos/main.rs"
        );
        assert_eq!(
            strip_verbatim_prefix("//?/UNC/servidor/share/main.rs".to_owned()),
            "//servidor/share/main.rs"
        );
        assert_eq!(
            strip_verbatim_prefix("/tmp/main.rs".to_owned()),
            "/tmp/main.rs"
        );
    }

    #[test]
    fn non_file_uris_are_rejected() {
        assert!(uri_to_path("https://example.com/a.rs").is_none());
        assert!(uri_to_path("file:///tmp/%ZZ.rs").is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn localhost_authority_is_accepted() {
        assert_eq!(
            uri_to_path("file://localhost/tmp/a.rs").as_deref(),
            Some(Path::new("/tmp/a.rs"))
        );
    }
}
