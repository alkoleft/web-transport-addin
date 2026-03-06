use std::collections::HashMap;
use std::sync::Arc;

use jsonschema::Validator;
use rmcp::model::{Prompt, Resource, ResourceTemplate, Tool};
use serde_json::{Map as JsonMap, Value as JsonValue};

use super::resource_template::{ResourceTemplateError, ResourceTemplateMatcher};

#[derive(Clone)]
pub struct ToolEntry {
    pub tool: Tool,
    pub schema: Arc<Validator>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceEntry {
    pub resource: Resource,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceTemplateEntry {
    pub template: ResourceTemplate,
    matcher: ResourceTemplateMatcher,
}

#[derive(Clone)]
pub struct PromptEntry {
    pub prompt: Prompt,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchedResourceTemplate {
    pub template: ResourceTemplate,
    pub arguments: JsonMap<String, JsonValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedResource {
    Resource(ResourceEntry),
    Template(MatchedResourceTemplate),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveResourceError {
    AmbiguousTemplates { uri: String, templates: Vec<String> },
}

#[derive(Default)]
pub struct Registry {
    tools: HashMap<String, ToolEntry>,
    resources: HashMap<String, ResourceEntry>,
    resource_templates: HashMap<String, ResourceTemplateEntry>,
    prompts: HashMap<String, PromptEntry>,
}

impl Registry {
    pub fn register_tool(&mut self, tool: Tool, schema: Validator) {
        let key = tool.name.to_string();
        self.tools.insert(
            key,
            ToolEntry {
                tool,
                schema: Arc::new(schema),
            },
        );
    }

    pub fn remove_tool(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    pub fn clear_tools(&mut self) {
        self.tools.clear();
    }

    pub fn get_tool(&self, name: &str) -> Option<ToolEntry> {
        self.tools.get(name).cloned()
    }

    pub fn list_tools(&self) -> Vec<Tool> {
        self.tools
            .values()
            .map(|entry| entry.tool.clone())
            .collect()
    }

    pub fn register_resource(&mut self, resource: Resource) {
        let key = resource.uri.clone();
        self.resources.insert(key, ResourceEntry { resource });
    }

    pub fn remove_resource(&mut self, uri: &str) -> bool {
        self.resources.remove(uri).is_some()
    }

    pub fn clear_resources(&mut self) {
        self.resources.clear();
    }

    pub fn get_resource(&self, uri: &str) -> Option<ResourceEntry> {
        self.resources.get(uri).cloned()
    }

    pub fn list_resources(&self) -> Vec<Resource> {
        self.resources
            .values()
            .map(|entry| entry.resource.clone())
            .collect()
    }

    pub fn register_resource_template(
        &mut self,
        template: ResourceTemplate,
    ) -> Result<(), ResourceTemplateError> {
        let key = template.uri_template.clone();
        let matcher = ResourceTemplateMatcher::compile(key.as_str())?;
        self.resource_templates
            .insert(key, ResourceTemplateEntry { template, matcher });
        Ok(())
    }

    pub fn remove_resource_template(&mut self, uri_template: &str) -> bool {
        self.resource_templates.remove(uri_template).is_some()
    }

    pub fn clear_resource_templates(&mut self) {
        self.resource_templates.clear();
    }

    pub fn list_resource_templates(&self) -> Vec<ResourceTemplate> {
        self.resource_templates
            .values()
            .map(|entry| entry.template.clone())
            .collect()
    }

    pub fn resolve_resource(
        &self,
        uri: &str,
    ) -> Result<Option<ResolvedResource>, ResolveResourceError> {
        if let Some(resource) = self.get_resource(uri) {
            return Ok(Some(ResolvedResource::Resource(resource)));
        }

        let mut matches = self
            .resource_templates
            .values()
            .filter_map(|entry| {
                entry
                    .matcher
                    .capture(uri)
                    .map(|arguments| MatchedResourceTemplate {
                        template: entry.template.clone(),
                        arguments,
                    })
            })
            .collect::<Vec<_>>();

        if matches.is_empty() {
            return Ok(None);
        }

        if matches.len() > 1 {
            let mut templates = matches
                .drain(..)
                .map(|matched| matched.template.uri_template.clone())
                .collect::<Vec<_>>();
            templates.sort();
            return Err(ResolveResourceError::AmbiguousTemplates {
                uri: uri.to_owned(),
                templates,
            });
        }

        Ok(matches.pop().map(ResolvedResource::Template))
    }

    pub fn register_prompt(&mut self, prompt: Prompt) {
        let key = prompt.name.clone();
        self.prompts.insert(key, PromptEntry { prompt });
    }

    pub fn remove_prompt(&mut self, name: &str) -> bool {
        self.prompts.remove(name).is_some()
    }

    pub fn clear_prompts(&mut self) {
        self.prompts.clear();
    }

    pub fn get_prompt(&self, name: &str) -> Option<PromptEntry> {
        self.prompts.get(name).cloned()
    }

    pub fn list_prompts(&self) -> Vec<Prompt> {
        self.prompts
            .values()
            .map(|entry| entry.prompt.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Registry, ResolveResourceError, ResolvedResource};
    use rmcp::model::{Resource, ResourceTemplate};
    use serde_json::json;

    fn resource(uri: &str) -> Resource {
        serde_json::from_value(json!({
            "uri": uri,
            "name": uri,
        }))
        .expect("valid resource")
    }

    fn resource_template(uri_template: &str) -> ResourceTemplate {
        serde_json::from_value(json!({
            "uriTemplate": uri_template,
            "name": uri_template,
        }))
        .expect("valid resource template")
    }

    #[test]
    fn registers_lists_and_removes_resource_templates() {
        let mut registry = Registry::default();
        let template = resource_template("str://users/{id}");

        registry
            .register_resource_template(template.clone())
            .expect("template should register");

        assert_eq!(registry.list_resource_templates(), vec![template.clone()]);
        assert!(registry.remove_resource_template(template.uri_template.as_str()));
        assert!(registry.list_resource_templates().is_empty());
    }

    #[test]
    fn clear_resource_templates_removes_all_entries() {
        let mut registry = Registry::default();
        registry
            .register_resource_template(resource_template("str://users/{id}"))
            .expect("template should register");
        registry
            .register_resource_template(resource_template("str://posts/{id}"))
            .expect("template should register");

        registry.clear_resource_templates();

        assert!(registry.list_resource_templates().is_empty());
    }

    #[test]
    fn exact_resource_wins_over_template_match() {
        let mut registry = Registry::default();
        registry.register_resource(resource("str://users/42"));
        registry
            .register_resource_template(resource_template("str://users/{id}"))
            .expect("template should register");

        let resolved = registry
            .resolve_resource("str://users/42")
            .expect("resolution should succeed")
            .expect("resource should resolve");

        assert!(matches!(resolved, ResolvedResource::Resource(_)));
    }

    #[test]
    fn resolves_single_matching_template() {
        let mut registry = Registry::default();
        registry
            .register_resource_template(resource_template("str://users/{id}"))
            .expect("template should register");

        let resolved = registry
            .resolve_resource("str://users/42")
            .expect("resolution should succeed")
            .expect("resource should resolve");

        match resolved {
            ResolvedResource::Template(template) => {
                assert_eq!(template.template.uri_template, "str://users/{id}");
                assert_eq!(
                    template.arguments,
                    json!({ "id": "42" })
                        .as_object()
                        .cloned()
                        .expect("json object"),
                );
            }
            other => panic!("expected template match, got {other:?}"),
        }
    }

    #[test]
    fn reports_ambiguous_template_matches() {
        let mut registry = Registry::default();
        registry
            .register_resource_template(resource_template("str://users/{id}"))
            .expect("template should register");
        registry
            .register_resource_template(resource_template("str://{kind}/{id}"))
            .expect("template should register");

        let error = registry
            .resolve_resource("str://users/42")
            .expect_err("resolution should fail");

        assert_eq!(
            error,
            ResolveResourceError::AmbiguousTemplates {
                uri: "str://users/42".to_owned(),
                templates: vec![
                    "str://users/{id}".to_owned(),
                    "str://{kind}/{id}".to_owned()
                ],
            }
        );
    }
}
