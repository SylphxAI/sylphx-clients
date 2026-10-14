//! Agent-native knowledge access reuses the generated method catalog and SDK.
use serde_json::{json, Map, Value};
use sylphx::Transport;

use crate::{tool_error, tool_ok, Server};

impl<T: Transport> Server<T> {
    pub(crate) fn agent_knowledge_tools(&self) -> Vec<Value> {
        let mut tools = Vec::new();
        if let Some(entry) = self.entry("knowledge.entities.recall") {
            tools.push(json!({"name":"knowledge_search","description":
                "Search readable Knowledge spaces with bounded graph expansion, episode citations and claim freshness. Re-read sources of stale claims.",
                "inputSchema":entry["input_schema"],"annotations":{"readOnlyHint":true,
                    "destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}));
        }
        if self.entry("agents.agent_versions.get").is_some() {
            tools.push(json!({"name":"skill_load","description":
                "Load one skill from an immutable AgentVersion. Use the version pinned by your session, not the agent's current version.",
                "inputSchema":{"type":"object","properties":{
                    "agent_version":{"type":"string","description":"Canonical immutable AgentVersion resource name."},
                    "name":{"type":"string","description":"Skill name in that version."}},
                    "required":["agent_version","name"],"additionalProperties":false},
                "annotations":{"readOnlyHint":true,"destructiveHint":false,
                    "idempotentHint":true,"openWorldHint":false}}));
        }
        tools
    }

    pub(crate) async fn call_agent_knowledge(&self, name: &str, args: Map<String, Value>) -> Value {
        if !self
            .agent_knowledge_tools()
            .iter()
            .any(|tool| tool["name"] == name)
        {
            return tool_error(
                "This tool's public service is not served or permitted on this MCP connection.",
            );
        }
        if name == "knowledge_search" {
            return self
                .call_method("knowledge.entities.recall", Value::Object(args), false)
                .await;
        }
        if args
            .keys()
            .any(|key| key != "agent_version" && key != "name")
        {
            return tool_error("skill_load arguments contain an unknown field.");
        }
        let version = args
            .get("agent_version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let skill_name = args.get("name").and_then(Value::as_str).unwrap_or_default();
        let segments: Vec<_> = version.split('/').collect();
        if !matches!(segments.as_slice(), ["orgs",org,"projects",project,"envs",env,
            "agents",agent,"agent_versions",revision] if !org.is_empty() && !project.is_empty()
            && !env.is_empty() && !agent.is_empty() && !revision.is_empty())
            || skill_name.is_empty()
        {
            return tool_error("skill_load needs an immutable AgentVersion name and a skill name.");
        }
        let response = self
            .call_method("agents.agent_versions.get", json!({"name":version}), false)
            .await;
        if response["isError"] == true {
            return response;
        }
        let Some(skills) = response["structuredContent"]["spec"]["skills"].as_array() else {
            return tool_error("The pinned version contains no skills.");
        };
        match skills.iter().find(|skill| skill["name"] == skill_name) {
            Some(skill) => tool_ok(json!({"agent_version":version,"skill":skill})),
            None => tool_error("The skill is not present in this immutable AgentVersion."),
        }
    }
}
