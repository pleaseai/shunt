use prost::Message;

// ---------------------------------------------------------------------------
// Agent client message (request)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct AgentClientMessage {
    #[prost(message, optional, tag = "1")]
    pub run_request: Option<RunRequest>,
    #[prost(message, optional, tag = "2")]
    pub client_heartbeat: Option<ClientHeartbeat>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RunRequest {
    #[prost(message, optional, tag = "1")]
    pub conversation_state: Option<ConversationState>,
    #[prost(message, optional, tag = "2")]
    pub action: Option<Action>,
    #[prost(message, optional, tag = "6")]
    pub mcp_tools: Option<McpTools>,
    #[prost(string, tag = "7")]
    pub conversation_id: String,
    #[prost(message, optional, tag = "8")]
    pub requested_model: Option<CursorModel>,
    #[prost(bool, tag = "9")]
    pub exclude_workspace_context: bool,
    #[prost(message, repeated, tag = "10")]
    pub selected_subagent_models: Vec<CursorModelRequest>,
    #[prost(string, tag = "11")]
    pub conversation_group_id: String,
    #[prost(bool, tag = "12")]
    pub client_supports_inline_images: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConversationState {
    #[prost(message, repeated, tag = "1")]
    pub messages: Vec<ConversationMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConversationMessage {
    #[prost(string, tag = "1")]
    pub role: String,
    #[prost(string, tag = "2")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Action {
    #[prost(message, optional, tag = "1")]
    pub user_message_action: Option<UserMessageAction>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UserMessageAction {
    #[prost(message, optional, tag = "1")]
    pub user_message: Option<UserMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UserMessage {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(string, tag = "2")]
    pub message_id: String,
    #[prost(message, optional, tag = "3")]
    pub selected_context: Option<SelectedContext>,
    #[prost(string, tag = "4")]
    pub mode: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SelectedContext {
    #[prost(message, repeated, tag = "1")]
    pub selected_images: Vec<SelectedImage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct McpTools {
    #[prost(message, repeated, tag = "1")]
    pub tools: Vec<McpTool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct McpTool {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    #[prost(string, tag = "3")]
    pub input_schema: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientHeartbeat {}

#[derive(Clone, PartialEq, Message)]
pub struct CursorModel {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(message, repeated, tag = "2")]
    pub parameters: Vec<ModelParameter>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CursorModelRequest {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(message, repeated, tag = "2")]
    pub parameters: Vec<ModelParameter>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ModelParameter {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SelectedImage {
    #[prost(string, tag = "1")]
    pub data: String,
    #[prost(string, tag = "2")]
    pub uuid: String,
    #[prost(string, tag = "3")]
    pub path: String,
    #[prost(string, tag = "4")]
    pub mime_type: String,
}

// ---------------------------------------------------------------------------
// Agent server message (response)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct AgentServerMessage {
    #[prost(message, optional, tag = "1")]
    pub interaction_update: Option<InteractionUpdate>,
    #[prost(message, optional, tag = "3")]
    pub exec_server_message: Option<ExecServerMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InteractionUpdate {
    #[prost(message, optional, tag = "1")]
    pub thinking_delta: Option<ThinkingDelta>,
    #[prost(message, optional, tag = "2")]
    pub text_delta: Option<TextDelta>,
    #[prost(message, optional, tag = "3")]
    pub turn_ended: Option<TurnEnded>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ThinkingDelta {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct TextDelta {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct TurnEnded {
    #[prost(uint64, tag = "1")]
    pub input_tokens: u64,
    #[prost(uint64, tag = "2")]
    pub output_tokens: u64,
    #[prost(uint64, tag = "3")]
    pub cache_read_tokens: u64,
    #[prost(uint64, tag = "4")]
    pub cache_write_tokens: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecServerMessage {
    #[prost(string, optional, tag = "1")]
    pub notes_session_id: Option<String>,
}
