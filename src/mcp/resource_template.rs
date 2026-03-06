use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Variable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceTemplateMatcher {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceTemplateError {
    message: String,
}

impl ResourceTemplateError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ResourceTemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ResourceTemplateError {}

impl ResourceTemplateMatcher {
    pub fn compile(uri_template: &str) -> Result<Self, ResourceTemplateError> {
        let mut segments = Vec::new();
        let mut cursor = 0usize;

        while let Some(relative_start) = uri_template[cursor..].find('{') {
            let start = cursor + relative_start;
            let literal = &uri_template[cursor..start];
            if literal.contains('}') {
                return Err(ResourceTemplateError::new(
                    "uriTemplate contains an unmatched `}`",
                ));
            }
            if !literal.is_empty() {
                segments.push(Segment::Literal(literal.to_owned()));
            } else if matches!(segments.last(), Some(Segment::Variable(_))) {
                return Err(ResourceTemplateError::new(
                    "uriTemplate does not support adjacent expressions",
                ));
            }

            let expression_start = start + 1;
            let Some(relative_end) = uri_template[expression_start..].find('}') else {
                return Err(ResourceTemplateError::new(
                    "uriTemplate contains an unmatched `{`",
                ));
            };
            let expression_end = expression_start + relative_end;
            let expression = &uri_template[expression_start..expression_end];
            if expression.contains('{') {
                return Err(ResourceTemplateError::new(
                    "uriTemplate contains nested expressions",
                ));
            }

            segments.push(Segment::Variable(parse_variable_name(expression)?));
            cursor = expression_end + 1;
        }

        let tail = &uri_template[cursor..];
        if tail.contains('}') {
            return Err(ResourceTemplateError::new(
                "uriTemplate contains an unmatched `}`",
            ));
        }
        if !tail.is_empty() || segments.is_empty() {
            segments.push(Segment::Literal(tail.to_owned()));
        }

        Ok(Self { segments })
    }

    pub fn capture(&self, uri: &str) -> Option<Map<String, Value>> {
        let mut remaining = uri;
        let mut arguments = Map::new();

        for (index, segment) in self.segments.iter().enumerate() {
            match segment {
                Segment::Literal(literal) => {
                    if !remaining.starts_with(literal) {
                        return None;
                    }
                    remaining = &remaining[literal.len()..];
                }
                Segment::Variable(name) => {
                    let value = match self.segments.get(index + 1) {
                        Some(Segment::Literal(next_literal)) => {
                            let position = remaining.find(next_literal)?;
                            let captured = &remaining[..position];
                            remaining = &remaining[position..];
                            captured
                        }
                        Some(Segment::Variable(_)) => return None,
                        None => {
                            let captured = remaining;
                            remaining = "";
                            captured
                        }
                    };

                    if let Some(existing) = arguments.get(name) {
                        if existing != value {
                            return None;
                        }
                    } else {
                        arguments.insert(name.clone(), Value::String(value.to_owned()));
                    }
                }
            }
        }

        if remaining.is_empty() {
            Some(arguments)
        } else {
            None
        }
    }
}

fn parse_variable_name(expression: &str) -> Result<String, ResourceTemplateError> {
    if expression.is_empty() {
        return Err(ResourceTemplateError::new(
            "uriTemplate contains an empty expression",
        ));
    }

    if expression.contains(':') || expression.contains('*') || expression.contains(',') {
        return Err(ResourceTemplateError::new(
            "uriTemplate modifiers are not supported; use plain `{var}` expressions",
        ));
    }

    const UNSUPPORTED_OPERATORS: &str = "+#./;?&=,!@|";
    if expression
        .chars()
        .next()
        .is_some_and(|character| UNSUPPORTED_OPERATORS.contains(character))
    {
        return Err(ResourceTemplateError::new(
            "uriTemplate operators are not supported; use plain `{var}` expressions",
        ));
    }

    if !expression.chars().all(is_supported_variable_char) {
        return Err(ResourceTemplateError::new(
            "uriTemplate contains an unsupported variable name",
        ));
    }

    Ok(expression.to_owned())
}

fn is_supported_variable_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
}

#[cfg(test)]
mod tests {
    use super::ResourceTemplateMatcher;
    use serde_json::{json, Map, Value};

    #[test]
    fn compiles_simple_template() {
        let matcher =
            ResourceTemplateMatcher::compile("str://users/{id}").expect("template should compile");

        let captures = matcher.capture("str://users/42").expect("uri should match");
        let expected = Map::from_iter([("id".to_owned(), Value::String("42".to_owned()))]);
        assert_eq!(captures, expected);
    }

    #[test]
    fn rejects_unsupported_rfc6570_features() {
        for template in [
            "str://users/{+id}",
            "str://users/{id*}",
            "str://users/{id:3}",
        ] {
            let error = ResourceTemplateMatcher::compile(template)
                .expect_err("unsupported template should fail");
            assert!(
                error.to_string().contains("plain `{var}`"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn extracts_multiple_arguments() {
        let matcher = ResourceTemplateMatcher::compile("str://users/{userId}/posts/{postId}")
            .expect("template should compile");

        let captures = matcher
            .capture("str://users/alice/posts/17")
            .expect("uri should match");
        assert_eq!(
            captures,
            json!({
                "userId": "alice",
                "postId": "17",
            })
            .as_object()
            .cloned()
            .expect("json object"),
        );
    }

    #[test]
    fn returns_none_for_non_matching_uri() {
        let matcher =
            ResourceTemplateMatcher::compile("str://users/{id}").expect("template should compile");

        assert!(matcher.capture("str://posts/42").is_none());
    }

    #[test]
    fn requires_repeated_variables_to_capture_same_value() {
        let matcher = ResourceTemplateMatcher::compile("str://compare/{id}/again/{id}")
            .expect("template should compile");

        assert!(matcher.capture("str://compare/42/again/42").is_some());
        assert!(matcher.capture("str://compare/42/again/99").is_none());
    }
}
