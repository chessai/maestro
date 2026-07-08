use scraper::{Html, Selector};

/// Strip `<script>` and `<style>` tags and HTML markup, returning visible text.
/// Deterministic and collapses whitespace for clean offsets.
pub fn html_to_text(html: &str) -> String {
    let document = Html::parse_document(html);

    // Selectors for elements to skip
    let skip_script = Selector::parse("script").unwrap();
    let skip_style = Selector::parse("style").unwrap();

    // Collect all nodes to skip
    let mut skip_nodes = std::collections::HashSet::new();
    for node in document.select(&skip_script) {
        skip_nodes.insert(node.id());
    }
    for node in document.select(&skip_style) {
        skip_nodes.insert(node.id());
    }

    let mut text = String::new();

    // Walk all text nodes, skipping those inside script/style
    for node in document.root_element().descendants() {
        // Check if any ancestor should be skipped
        let mut should_skip = false;
        if let Some(text_node) = node.value().as_text() {
            let mut current = node;
            while let Some(parent) = current.parent() {
                if skip_nodes.contains(&parent.id()) {
                    should_skip = true;
                    break;
                }
                current = parent;
            }

            if !should_skip {
                let content = text_node.text.trim();
                if !content.is_empty() {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(content);
                }
            }
        }
    }

    // Collapse multiple spaces
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_html_to_text_removes_script() {
        let html = "<html><body><script>x=1</script><p>Hello world</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello world"));
        assert!(!text.contains("x=1"));
    }

    #[test]
    fn test_html_to_text_removes_style() {
        let html = "<html><body><style>.foo { color: red; }</style><p>Hello world</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello world"));
        assert!(!text.contains("color"));
    }

    #[test]
    fn test_html_to_text_extracts_visible_text() {
        let html = "<div><p>First</p><p>Second</p></div>";
        let text = html_to_text(html);
        assert!(text.contains("First"));
        assert!(text.contains("Second"));
    }

    #[test]
    fn test_html_to_text_collapses_whitespace() {
        let html = "<div>\n  Hello   \n\n  world  \n</div>";
        let text = html_to_text(html);
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn test_html_to_text_deterministic() {
        let html = "<html><body><p>Test</p></body></html>";
        let text1 = html_to_text(html);
        let text2 = html_to_text(html);
        assert_eq!(text1, text2);
    }
}
