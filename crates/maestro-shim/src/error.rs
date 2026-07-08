#[derive(Debug, thiserror::Error)]
pub enum ShimError {
    #[error("search backend unavailable: {0}")]
    BackendUnavailable(String),

    #[error("http error: {0}")]
    Http(String),

    #[error("extraction model unavailable: {0}")]
    ModelUnavailable(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("fabricated offset for field {field}: content[{start}..{end}] does not equal the claimed verbatim")]
    FabricatedOffset {
        field: String,
        start: usize,
        end: usize,
    },

    #[error("rejected: verbatim for field {field} does not occur in the fetched content")]
    VerbatimNotFound { field: String },
}
