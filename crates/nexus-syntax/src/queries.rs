//! Consultas de highlighting embutidas.
//!
//! Foram adaptadas das consultas oficiais das gramáticas (licença MIT),
//! removendo padrões que dependem de predicados de texto. A classificação por
//! convenção de nome é feita pelo engine, de forma determinística.
//!
//! Para adicionar uma linguagem, crie uma consulta `.scm` nesta pasta e
//! registre-a em [`crate::language`].

pub(crate) const RUST: &str = include_str!("../queries/rust.scm");
pub(crate) const PYTHON: &str = include_str!("../queries/python.scm");
pub(crate) const JAVASCRIPT: &str = include_str!("../queries/javascript.scm");
pub(crate) const TYPESCRIPT: &str = include_str!("../queries/typescript.scm");
