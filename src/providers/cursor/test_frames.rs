use super::connect::encode_connect_frame;

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while value >= 0x80 {
        out.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
    out
}

fn bytes_field(field: u64, value: &[u8]) -> Vec<u8> {
    let mut out = varint((field << 3) | 2);
    out.extend(varint(value.len() as u64));
    out.extend(value);
    out
}

fn varint_field(field: u64, value: u64) -> Vec<u8> {
    let mut out = varint(field << 3);
    out.extend(varint(value));
    out
}

fn interaction_frame(update_field: u64, payload: &[u8]) -> Vec<u8> {
    let update = bytes_field(update_field, payload);
    encode_connect_frame(bytes_field(1, &update), 0).to_vec()
}

pub(crate) fn text_frame(text: &str) -> Vec<u8> {
    interaction_frame(1, &bytes_field(1, text.as_bytes()))
}

pub(crate) fn thinking_frame(text: &str) -> Vec<u8> {
    interaction_frame(4, &bytes_field(1, text.as_bytes()))
}

pub(crate) fn usage_frame(input: u64, output: u64) -> Vec<u8> {
    usage_frame_full(input, output, 0, 0)
}

pub(crate) fn usage_frame_full(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> Vec<u8> {
    let mut usage = varint_field(1, input);
    usage.extend(varint_field(2, output));
    usage.extend(varint_field(3, cache_read));
    usage.extend(varint_field(4, cache_write));
    interaction_frame(14, &usage)
}

/// The frame a Cursor Read tool call arrives in: AgentServerMessage tag 2
/// (exec_server_message) -> read_args (tag 7) -> path (tag 1), tool_call_id
/// (tag 2), with exec_id at tag 15. Field numbers measured from the live
/// upstream 2026-10-12.
pub(crate) fn native_read_exec_frame(path: &str, tool_call_id: &str, exec_id: &str) -> Vec<u8> {
    let mut read_args = bytes_field(1, path.as_bytes());
    read_args.extend(bytes_field(2, tool_call_id.as_bytes()));
    let mut exec = varint_field(1, 1);
    exec.extend(bytes_field(7, &read_args));
    exec.extend(bytes_field(15, exec_id.as_bytes()));
    encode_connect_frame(bytes_field(2, &exec), 0).to_vec()
}

/// A grep exec request: AgentServerMessage tag 2 (exec_server_message) ->
/// grep_args (tag 5) -> pattern (tag 1), path (tag 2), with the exec id at
/// tag 15. Shapes from the Cursor Agent descriptors (agent/v1/grep_exec.proto).
pub(crate) fn native_grep_exec_frame(pattern: &str, path: &str, id: u64, exec_id: &str) -> Vec<u8> {
    let mut grep_args = bytes_field(1, pattern.as_bytes());
    grep_args.extend(bytes_field(2, path.as_bytes()));
    let mut exec = varint_field(1, id);
    exec.extend(bytes_field(5, &grep_args));
    exec.extend(bytes_field(15, exec_id.as_bytes()));
    encode_connect_frame(bytes_field(2, &exec), 0).to_vec()
}

/// InteractionUpdate tool_call_started (tag 2). The old proto mapping read this
/// as text_delta and sent Cursor's tool call to Claude Code as prose.
pub(crate) fn tool_call_started_frame(tool_call_id: &str) -> Vec<u8> {
    let mut started = bytes_field(1, tool_call_id.as_bytes());
    started.extend(bytes_field(
        3,
        b"be193bcf-0000-0000-0000-000000000000-0-aho4",
    ));
    interaction_frame(2, &started)
}

pub(crate) fn end_frame() -> Vec<u8> {
    encode_connect_frame(b"", 2).to_vec()
}
