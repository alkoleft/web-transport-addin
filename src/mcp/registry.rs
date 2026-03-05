use std::collections::HashMap;
use std::sync::Arc;

use jsonschema::Validator;
use rmcp::model::{Prompt, Resource, Tool};

#[derive(Clone)]
pub struct ToolEntry {
    pub tool: Tool,
    pub schema: Arc<Validator>,
}

#[derive(Clone)]
pub struct ResourceEntry {
    pub resource: Resource,
}

#[derive(Clone)]
pub struct PromptEntry {
    pub prompt: Prompt,
}

#[derive(Default)]
pub struct Registry {
    tools: HashMap<String, ToolEntry>,
    resources: HashMap<String, ResourceEntry>,
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
        self.tools.values().map(|entry| entry.tool.clone()).collect()
    }

    pub fn register_resource(&mut self, resource: Resource) {
        let key = resource.uri.clone();
        self.resources
            .insert(key, ResourceEntry { resource });
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
