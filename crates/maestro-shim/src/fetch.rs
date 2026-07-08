use crate::ShimError;

/// HTTP GET fetch: retrieve the body of a URL as a String.
/// ureq/HTTP errors are mapped to ShimError::Http.
pub fn fetch(url: &str) -> Result<String, ShimError> {
    ureq::get(url)
        .call()
        .map_err(|e| ShimError::Http(e.to_string()))?
        .into_string()
        .map_err(|e| ShimError::Http(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fetch_invalid_url() {
        // This will fail because the URL is not valid or unreachable
        let result = fetch("http://localhost:99999/nonexistent");
        assert!(result.is_err());
        match result.unwrap_err() {
            ShimError::Http(_) => (),
            _ => panic!("Expected Http error"),
        }
    }
}
