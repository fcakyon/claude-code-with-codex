use prost::Message;

// ---------------------------------------------------------------------------
// Agent client message (request)
// ---------------------------------------------------------------------------

// Envelope the client writes into the open `Run` request body. Tag numbers from
// the Cursor Agent descriptors (agent.proto, AgentClientMessage): 1 run_request,
// 2 exec_client_message, 5 exec_client_control_message, 7 client_heartbeat. The
// heartbeat used to sit on tag 2, which is the exec reply -- a reply carrying no
// result reads as "ran and produced nothing" and the server waits forever.
#[derive(Clone, PartialEq, Message)]
pub struct AgentClientMessage {
    #[prost(message, optional, tag = "1")]
    pub run_request: Option<RunRequest>,
    #[prost(message, optional, tag = "2")]
    pub exec_client_message: Option<ExecClientMessage>,
    #[prost(message, optional, tag = "3")]
    pub kv_client_message: Option<KvClientMessage>,
    #[prost(message, optional, tag = "5")]
    pub exec_client_control_message: Option<ExecClientControlMessage>,
    #[prost(message, optional, tag = "7")]
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
    #[prost(message, optional, tag = "2")]
    pub exec_server_message: Option<ExecServerMessage>,
    #[prost(message, optional, tag = "3")]
    pub conversation_checkpoint_update: Option<ConversationCheckpointUpdate>,
    #[prost(message, optional, tag = "4")]
    pub kv_server_message: Option<KvServerMessage>,
}

/// The server's conversation state, sent whole at each step. Only the token
/// counter is read: `token_details` (tag 5) is the size of the context the
/// model saw on its latest step, which is what Claude Code's context gauge
/// means. `TurnEnded` adds up every internal step of a run instead.
#[derive(Clone, PartialEq, Message)]
pub struct ConversationCheckpointUpdate {
    #[prost(message, optional, tag = "5")]
    pub token_details: Option<ConversationTokenDetails>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConversationTokenDetails {
    #[prost(uint32, tag = "1")]
    pub used_tokens: u32,
    #[prost(uint32, tag = "2")]
    pub max_tokens: u32,
}

/// The agent store: the server writes its conversation state here and reads it
/// back on resume. Every message expects a `KvClientMessage` on the same id.
#[derive(Clone, PartialEq, Message)]
pub struct KvServerMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "kv_server_message::Message", tags = "2, 3")]
    pub message: Option<kv_server_message::Message>,
}

pub mod kv_server_message {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        GetBlobArgs(super::GetBlobArgs),
        #[prost(message, tag = "3")]
        SetBlobArgs(super::SetBlobArgs),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct GetBlobArgs {
    #[prost(bytes, tag = "1")]
    pub blob_id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetBlobArgs {
    #[prost(bytes, tag = "1")]
    pub blob_id: Vec<u8>,
    #[prost(bytes, tag = "2")]
    pub blob_data: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct KvClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "kv_client_message::Message", tags = "2, 3")]
    pub message: Option<kv_client_message::Message>,
}

pub mod kv_client_message {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        GetBlobResult(super::GetBlobResult),
        #[prost(message, tag = "3")]
        SetBlobResult(super::SetBlobResult),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct GetBlobResult {
    #[prost(bytes, optional, tag = "1")]
    pub blob_data: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetBlobResult {}

// Tag numbers verified against the Cursor Agent bundle's protobuf descriptors
// (2026.10.01-e373342, 12/10/2026): 1 text_delta, 2 tool_call_started,
// 3 tool_call_completed, 4 thinking_delta, 14 turn_ended. The previous mapping
// (thinking 1, text 2, turn_ended 3) read tool_call_started as the model's text,
// which is why a Cursor tool call reached Claude Code as the literal string
// "call_...\nfc_..." instead of a tool_use block.
#[derive(Clone, PartialEq, Message)]
pub struct InteractionUpdate {
    #[prost(message, optional, tag = "1")]
    pub text_delta: Option<TextDelta>,
    #[prost(message, optional, tag = "4")]
    pub thinking_delta: Option<ThinkingDelta>,
    #[prost(message, optional, tag = "14")]
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

// The frame in which Cursor asks the CLIENT to run a tool. Field numbers from
// the same descriptors: 1 id, 2 shell_args, 3 write_args, 7 read_args,
// 14 shell_stream_args, 15 exec_id. Measured live 12/10/2026: a Read arrives as
// read_args{path, tool_call_id} and a Bash as shell_stream_args with the same
// ShellArgs shape.
#[derive(Clone, PartialEq, Message)]
pub struct ExecServerMessage {
    #[prost(uint32, optional, tag = "1")]
    pub id: Option<u32>,
    #[prost(message, optional, tag = "2")]
    pub shell_args: Option<ShellArgs>,
    #[prost(message, optional, tag = "3")]
    pub write_args: Option<WriteArgs>,
    #[prost(message, optional, tag = "4")]
    pub delete_args: Option<DeleteArgs>,
    #[prost(message, optional, tag = "5")]
    pub grep_args: Option<GrepArgs>,
    #[prost(message, optional, tag = "7")]
    pub read_args: Option<ReadArgs>,
    #[prost(message, optional, tag = "8")]
    pub ls_args: Option<LsArgs>,
    #[prost(message, optional, tag = "10")]
    pub request_context_args: Option<RequestContextArgs>,
    #[prost(message, optional, tag = "14")]
    pub shell_stream_args: Option<ShellArgs>,
    #[prost(string, optional, tag = "15")]
    pub exec_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ReadArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, optional, tag = "2")]
    pub tool_call_id: Option<String>,
    #[prost(int32, optional, tag = "4")]
    pub offset: Option<i32>,
    #[prost(uint32, optional, tag = "5")]
    pub limit: Option<u32>,
    #[prost(string, optional, tag = "6")]
    pub encoding_hint: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WriteArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub file_text: String,
    #[prost(string, optional, tag = "3")]
    pub tool_call_id: Option<String>,
    #[prost(bool, tag = "4")]
    pub return_file_content_after_write: bool,
    /// A byte-backed write sets this and leaves `file_text` empty.
    #[prost(bytes, optional, tag = "5")]
    pub file_bytes: Option<Vec<u8>>,
    /// Charset of the payload (`utf8`, `latin1`, ...), not an encoding of it.
    #[prost(string, optional, tag = "6")]
    pub encoding_hint: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellArgs {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, optional, tag = "2")]
    pub working_directory: Option<String>,
    /// Milliseconds: the Cursor agent's own default is 30000 and it feeds the
    /// value straight to `setTimeout` (index.js, 2026.10.01 bundle).
    #[prost(int32, optional, tag = "3")]
    pub timeout: Option<i32>,
    #[prost(string, optional, tag = "4")]
    pub tool_call_id: Option<String>,
    #[prost(int32, optional, tag = "14")]
    pub hard_timeout: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, optional, tag = "2")]
    pub tool_call_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GrepArgs {
    #[prost(string, tag = "1")]
    pub pattern: String,
    #[prost(string, optional, tag = "2")]
    pub path: Option<String>,
    #[prost(string, optional, tag = "14")]
    pub tool_call_id: Option<String>,
}

/// The agent asking for workspace context. Measured live 12/10/2026: it is the
/// first exec of a run, and it carries no id.
#[derive(Clone, PartialEq, Message)]
pub struct RequestContextArgs {
    #[prost(string, optional, tag = "2")]
    pub notes_session_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct LsArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, optional, tag = "3")]
    pub tool_call_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Agent client exec reply (the answer the server waits for)
// ---------------------------------------------------------------------------

/// The reply to one `ExecServerMessage`, written back on the same open `Run`.
/// `id` and `exec_id` are echoed unchanged: the id space is shared with the
/// interactions and a result carrying an unknown id is dropped.
#[derive(Clone, PartialEq, Message)]
pub struct ExecClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(
        oneof = "exec_client_message::Message",
        tags = "2, 3, 4, 5, 7, 8, 10, 14"
    )]
    pub message: Option<exec_client_message::Message>,
}

pub mod exec_client_message {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        ShellResult(super::ShellResult),
        #[prost(message, tag = "3")]
        WriteResult(super::WriteResult),
        #[prost(message, tag = "4")]
        DeleteResult(super::DeleteResult),
        #[prost(message, tag = "5")]
        GrepResult(super::GrepResult),
        #[prost(message, tag = "7")]
        ReadResult(super::ReadResult),
        #[prost(message, tag = "8")]
        LsResult(super::LsResult),
        #[prost(message, tag = "10")]
        RequestContextResult(super::RequestContextResult),
        #[prost(message, tag = "14")]
        ShellStream(super::ShellStream),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecClientControlMessage {
    #[prost(oneof = "exec_client_control_message::Message", tags = "1, 2, 3")]
    pub message: Option<exec_client_control_message::Message>,
}

pub mod exec_client_control_message {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        StreamClose(super::ExecClientStreamClose),
        #[prost(message, tag = "2")]
        Throw(super::ExecClientThrow),
        #[prost(message, tag = "3")]
        Heartbeat(super::ExecClientHeartbeat),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecClientStreamClose {
    #[prost(uint32, tag = "1")]
    pub id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecClientThrow {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "2")]
    pub error: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecClientHeartbeat {
    #[prost(uint32, tag = "1")]
    pub id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ReadResult {
    #[prost(oneof = "read_result::Result", tags = "1, 2, 3")]
    pub result: Option<read_result::Result>,
}

pub mod read_result {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::ReadSuccess),
        #[prost(message, tag = "2")]
        Error(super::ReadError),
        #[prost(message, tag = "3")]
        Rejected(super::ReadRejected),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ReadSuccess {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(int32, tag = "3")]
    pub total_lines: i32,
    #[prost(int64, tag = "4")]
    pub file_size: i64,
    #[prost(bool, tag = "6")]
    pub truncated: bool,
    #[prost(oneof = "read_success::Output", tags = "2, 5")]
    pub output: Option<read_success::Output>,
}

pub mod read_success {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Output {
        #[prost(string, tag = "2")]
        Content(String),
        #[prost(bytes, tag = "5")]
        Data(Vec<u8>),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ReadError {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub error: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ReadRejected {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub reason: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct WriteResult {
    #[prost(oneof = "write_result::Result", tags = "1, 5, 6")]
    pub result: Option<write_result::Result>,
}

pub mod write_result {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::WriteSuccess),
        #[prost(message, tag = "5")]
        Error(super::WriteError),
        #[prost(message, tag = "6")]
        Rejected(super::WriteRejected),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct WriteSuccess {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(int32, tag = "2")]
    pub lines_created: i32,
    #[prost(int32, tag = "3")]
    pub file_size: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct WriteError {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub error: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct WriteRejected {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub reason: String,
}

/// `GrepResult` models only the error outcome here: the bridge has no Claude
/// Grep mapping, and a result the bridge never constructs only has to encode.
#[derive(Clone, PartialEq, Message)]
pub struct GrepResult {
    #[prost(oneof = "grep_result::Result", tags = "2")]
    pub result: Option<grep_result::Result>,
}

pub mod grep_result {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "2")]
        Error(super::GrepError),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct GrepError {
    #[prost(string, tag = "1")]
    pub error: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct LsResult {
    #[prost(oneof = "ls_result::Result", tags = "3")]
    pub result: Option<ls_result::Result>,
}

pub mod ls_result {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "3")]
        Rejected(super::LsRejected),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct LsRejected {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub reason: String,
}

/// The answer to `request_context_args` (tag 10). The agent asks for workspace
/// context before it plans; this bridge renders its own prompt and has none to
/// add, but the request still needs a reply.
#[derive(Clone, PartialEq, Message)]
pub struct RequestContextResult {
    #[prost(oneof = "request_context_result::Result", tags = "1")]
    pub result: Option<request_context_result::Result>,
}

pub mod request_context_result {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::RequestContextSuccess),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct RequestContext {
    #[prost(message, optional, tag = "4")]
    pub env: Option<RequestContextEnv>,
}

/// The environment the agent is told it runs in. Measured 12/10/2026: an answer
/// to `request_context_args` without this block makes the server fail the run
/// with "Failed to get request context (internal)".
#[derive(Clone, PartialEq, Message)]
pub struct RequestContextEnv {
    #[prost(string, tag = "1")]
    pub os_version: String,
    #[prost(string, repeated, tag = "2")]
    pub workspace_paths: Vec<String>,
    #[prost(string, tag = "3")]
    pub shell: String,
    #[prost(string, tag = "10")]
    pub time_zone: String,
    #[prost(string, tag = "11")]
    pub project_folder: String,
    #[prost(bool, optional, tag = "14")]
    pub sandbox_supported: Option<bool>,
    #[prost(bool, optional, tag = "16")]
    pub sandbox_network_has_defaults: Option<bool>,
    #[prost(string, optional, tag = "21")]
    pub process_working_directory: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RequestContextSuccess {
    #[prost(message, optional, tag = "1")]
    pub request_context: Option<RequestContext>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteResult {
    #[prost(oneof = "delete_result::Result", tags = "6")]
    pub result: Option<delete_result::Result>,
}

pub mod delete_result {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "6")]
        Rejected(super::DeleteRejected),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteRejected {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub reason: String,
}

/// One message of a shell execution stream. A stream is a SEQUENCE of these
/// with the same `id`: start, stdout/stderr, then exit. Without the exit the
/// server never closes the execution.
#[derive(Clone, PartialEq, Message)]
pub struct ShellStream {
    #[prost(oneof = "shell_stream::Event", tags = "1, 2, 3, 4")]
    pub event: Option<shell_stream::Event>,
}

pub mod shell_stream {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        Stdout(super::ShellStreamStdout),
        #[prost(message, tag = "2")]
        Stderr(super::ShellStreamStderr),
        #[prost(message, tag = "3")]
        Exit(super::ShellStreamExit),
        #[prost(message, tag = "4")]
        Start(super::ShellStreamStart),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellStreamStdout {
    #[prost(string, tag = "1")]
    pub data: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellStreamStderr {
    #[prost(string, tag = "1")]
    pub data: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellStreamExit {
    #[prost(uint32, tag = "1")]
    pub code: u32,
    #[prost(string, tag = "2")]
    pub cwd: String,
    #[prost(bool, tag = "4")]
    pub aborted: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellStreamStart {}

/// A final structured shell result. Cursor can keep the turn pending when it
/// receives only stream deltas, so the exit is followed by this acknowledgement.
#[derive(Clone, PartialEq, Message)]
pub struct ShellResult {
    #[prost(oneof = "shell_result::Result", tags = "1, 2")]
    pub result: Option<shell_result::Result>,
}

pub mod shell_result {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::ShellSuccess),
        #[prost(message, tag = "2")]
        Failure(super::ShellFailure),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellSuccess {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub working_directory: String,
    #[prost(int32, tag = "3")]
    pub exit_code: i32,
    #[prost(string, tag = "5")]
    pub stdout: String,
    #[prost(string, tag = "6")]
    pub stderr: String,
    #[prost(int32, tag = "7")]
    pub execution_time: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ShellFailure {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub working_directory: String,
    #[prost(int32, tag = "3")]
    pub exit_code: i32,
    #[prost(string, tag = "5")]
    pub stdout: String,
    #[prost(string, tag = "6")]
    pub stderr: String,
    #[prost(int32, tag = "7")]
    pub execution_time: i32,
}
